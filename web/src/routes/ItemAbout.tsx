/**
 * An item's details (directors, cast, languages, links…) and where each came
 * from: the file, a provider, a person, or an agent with the pages it used.
 * Read through the same operation agents use (`item.get`), as a person: the
 * server withholds nothing from this page. "Fix with your agent…" copies a
 * brief for the user's own agent, as "Make a lens" does for lenses.
 */
import { useState } from 'react';
import { Link } from '../app/Link';
import { countryName, fieldName, hostOf, languageName, type AgentItemDetail, type FieldSource, type Fields } from '../api/agent';
import { formatAgo } from '../api/format';
import type { UseQueryResult } from '../api/query';
import type { ItemDetail } from '../api/types';
import { Icon } from '../components/Icon';
import styles from './Item.module.css';
import about from './ItemAbout.module.css';
import { agentBrief, formatRelease } from './item-brief';

/** Who set a value, in a few words. */
function setBy(s: FieldSource): string {
  if (s.source === 'agent') return s.agent ?? 'an agent';
  if (s.source === 'user') return s.by ?? 'a person';
  if (s.source === 'provider') return (s.provider ?? 'a provider').toUpperCase();
  return 'the file';
}

function SourceMark({ s }: { s: FieldSource | undefined }) {
  if (!s || (s.source !== 'agent' && s.source !== 'user')) return null;
  const from = s.sources.map((l) => hostOf(l.url)).filter((h, i, a) => a.indexOf(h) === i);
  const title = `Set by ${setBy(s)}${s.at ? `, ${formatAgo(s.at)}` : ''}${from.length ? `, from ${from.join(', ')}` : ''}`;
  // A quiet mark per value; "Where this comes from" names who and how.
  const chip = (
    <span className={about.mark} data-source={s.source}>
      {s.source === 'agent' ? <Icon name="sparkle" /> : <Icon name="user" />}
    </span>
  );
  return s.changeset ? (
    <Link to={`/changes?open=${s.changeset}`} className={about.markLink} title={title} aria-label={title}>
      {chip}
    </Link>
  ) : (
    <span title={title}>{chip}</span>
  );
}

type Row = { field: keyof Fields; label: string; value: string };

function rows(f: Fields): Row[] {
  const out: Row[] = [];
  const list = (field: keyof Fields, label: string, v: string[] | null, map: (s: string) => string = (s) => s) => {
    if (v && v.length) out.push({ field, label, value: v.map(map).join(', ') });
  };
  list('directors', 'Directed by', f.directors);
  list('writers', 'Written by', f.writers);
  if (f.cast?.length) {
    const top = f.cast.slice(0, 6).map((c) => (c.role ? `${c.name} (${c.role})` : c.name));
    out.push({ field: 'cast', label: 'Cast', value: top.join(', ') + (f.cast.length > 6 ? `, and ${f.cast.length - 6} more` : '') });
  }
  list('studios', 'Studio', f.studios);
  if (f.release_date) out.push({ field: 'release_date', label: 'Released', value: formatRelease(f.release_date) });
  if (f.air_date) out.push({ field: 'air_date', label: 'Aired', value: formatRelease(f.air_date) });
  list('countries', 'Country', f.countries, countryName);
  list('languages', 'Language', f.languages, languageName);
  if (f.content_rating) out.push({ field: 'content_rating', label: 'Rating', value: f.content_rating });
  list('tags', 'Tags', f.tags);
  return out;
}

/** Fields grouped by who set them (a changeset, a provider, a person). */
function provenance(sources: Record<string, FieldSource>) {
  const groups = new Map<string, { s: FieldSource; fields: string[] }>();
  for (const [field, s] of Object.entries(sources)) {
    if (s.source === 'file') continue;
    const key = s.changeset ? `cs:${s.changeset}` : `${s.source}:${s.provider ?? s.agent ?? s.by ?? ''}`;
    const g = groups.get(key) ?? { s, fields: [] };
    g.fields.push(field);
    groups.set(key, g);
  }
  return [...groups.values()].sort((a, b) => (b.s.at ?? 0) - (a.s.at ?? 0));
}

function FixWithAgent({ item, facts }: { item: ItemDetail; facts: AgentItemDetail | undefined }) {
  const [copied, setCopied] = useState(false);
  const origin = typeof location === 'undefined' ? '' : location.origin;
  return (
    <div className={about.fix}>
      <button
        type="button"
        className="btn btn-sm"
        onClick={() => {
          void navigator.clipboard?.writeText(agentBrief(item, facts, origin)).then(() => {
            setCopied(true);
            setTimeout(() => setCopied(false), 6000);
          });
        }}
      >
        <Icon name={copied ? 'check' : 'agent'} />
        {copied ? 'Brief copied' : 'Fix with your agent…'}
      </button>
      {copied && (
        <p className={about.hint} role="status">
          Paste it into your agent. Not connected yet? <Link to="/changes#connect">Connect one</Link>.
        </p>
      )}
    </div>
  );
}

export function AboutPanel({ item, facts }: { item: ItemDetail; facts: UseQueryResult<AgentItemDetail> }) {
  const f = facts.data?.fields;
  const sources = facts.data?.sources ?? {};
  const list = f ? rows(f) : [];
  const groups = provenance(sources);
  const links = f?.links ?? [];
  const kind = item.kind === 'show' ? 'show' : item.kind === 'episode' ? 'episode' : item.kind === 'season' ? 'season' : 'title';
  return (
    <section className={styles.panel} aria-labelledby="about">
      <h2 id="about" className={`edge ${styles.panelTitle}`}>
        About
      </h2>
      {f?.tagline && <p className={about.tagline}>{f.tagline}</p>}
      {!facts.data && facts.status !== 'error' && <div className={`skeleton ${about.skeleton}`} />}
      {facts.data && list.length === 0 && !f?.tagline && <p className={about.empty}>Nothing more is known about this {kind} yet.</p>}
      {list.length > 0 && (
        <dl className={about.facts}>
          {list.map((r) => (
            <div key={r.field}>
              <dt>{r.label}</dt>
              <dd>
                <span>{r.value}</span>
                <SourceMark s={sources[r.field]} />
              </dd>
            </div>
          ))}
        </dl>
      )}
      {links.length > 0 && (
        <p className={styles.ids}>
          {links.map((l) => (
            <a key={l.url} href={l.url} target="_blank" rel="noreferrer noopener" className={styles.idLink}>
              {l.title || hostOf(l.url)} <Icon name="link" />
            </a>
          ))}
        </p>
      )}
      {groups.length > 0 && (
        <>
          <h3 className={styles.subhead}>Where this comes from</h3>
          <ul className={about.origins}>
            {groups.map(({ s, fields }) => {
              const from = s.sources.map((l) => hostOf(l.url)).filter((h, i, a) => a.indexOf(h) === i);
              return (
                <li key={`${s.changeset ?? s.source}-${fields[0]}`} data-source={s.source}>
                  <span>
                    <b>{setBy(s)}</b>
                    {s.at ? ` · ${formatAgo(s.at)}` : ''}: {fields.map(fieldName).join(', ')}
                    {from.length > 0 && <span className={about.from}> · from {from.join(', ')}</span>}
                  </span>
                  {s.changeset && (
                    <Link to={`/changes?open=${s.changeset}`} className={about.review}>
                      Review
                    </Link>
                  )}
                </li>
              );
            })}
            <li data-source="file">
              <span>
                <b>The file</b>: {groups.length ? 'everything else' : 'all of it'}
              </span>
            </li>
          </ul>
        </>
      )}
      <FixWithAgent item={item} facts={facts.data} />
    </section>
  );
}
