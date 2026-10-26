/**
 * The mock server's agent operations (crates/flux-server/src/agent): enough
 * of `POST /api/v1/ops/{name}` and `GET /api/v1/agent` for the web client's
 * own uses (Changes, an item's details and sources, lenses kept on the
 * server), against a small journal seeded with what an agent might have
 * done to this library. Values an "agent" set here are written for the
 * mock, like curated.ts's loglines.
 */
import { delay, http, HttpResponse } from 'msw';
import type {
  AgentArt,
  AgentItem,
  AgentItemDetail,
  Change,
  CollectionInfo,
  CollectionItems,
  CollectionPick,
  CollectionRule,
  Collections,
  EditOutput,
  Changeset,
  ChangesetState,
  ChangesListOutput,
  Decided,
  ErrorCode,
  FieldSource,
  Fields,
  FileEvidence,
  Lenses,
  LensSpec,
  Link,
  Manifest,
  NotDone,
  StoredLens,
} from '../api/agent';
import type { FluxEvent, Id, ItemSummary, LibrarySort, Page } from '../api/types';
import { MATCHED_IDS } from './curated';
import type { MockDb } from './db';

interface Ctx {
  db: MockDb;
  u: (path: string) => string;
  latencyMs: number;
  broadcast: (e: FluxEvent) => void;
}

type Values = Record<string, unknown>;

const WIKI = (page: string, title = 'Wikipedia'): Link => ({ url: `https://en.wikipedia.org/wiki/${page}`, title });

/** Fields of a file plan's steps (they move files on disk). */
const PLAN_FIELDS = new Set(['file.path', 'folder.path', 'folder.new', 'folder.gone']);

const EMPTY_FIELDS: Fields = {
  title: null,
  original_title: null,
  year: null,
  overview: null,
  tagline: null,
  genres: null,
  tags: null,
  release_date: null,
  air_date: null,
  languages: null,
  countries: null,
  directors: null,
  writers: null,
  cast: null,
  studios: null,
  content_rating: null,
  links: null,
  ids: null,
};

function opError(status: number, code: ErrorCode, message: string) {
  return HttpResponse.json({ error: { code, message } }, { status });
}

export function agentHandlers(ctx: Ctx) {
  const { db, u } = ctx;
  const wait = () => (ctx.latencyMs > 0 ? delay(ctx.latencyMs) : Promise.resolve());
  /** What people and agents set, per item: values and their sources. */
  const set = new Map<Id, { values: Values; sources: Record<string, FieldSource> }>();
  const journal: Changeset[] = [];
  const lenses: StoredLens[] = [];
  let nextId = 1;
  const t0 = db.now;

  const find = (title: string): ItemSummary | undefined => db.search(title).find((s) => s.title === title);

  const record = (item: Id, field: string, value: unknown, src: FieldSource | null) => {
    const e = set.get(item) ?? { values: {}, sources: {} };
    if (value === null || value === undefined) delete e.values[field];
    else e.values[field] = value;
    if (src) e.sources[field] = src;
    else delete e.sources[field];
    set.set(item, e);
  };

  const valueOf = (item: Id, field: string): unknown => set.get(item)?.values[field] ?? null;

  /** Seed: a changeset as an agent would have made it. */
  const seed = (
    summary: string,
    state: ChangesetState,
    agent: string | null,
    by: string,
    ago: number,
    edits: { item: ItemSummary | undefined; values: Values; sources: Link[] }[],
  ) => {
    const id = nextId++;
    const changes: Change[] = [];
    for (const e of edits) {
      if (!e.item) continue;
      for (const [field, after] of Object.entries(e.values)) {
        changes.push({ item: e.item.id, title: e.item.title, field, before: null, after, withheld: null, sources: e.sources, state: state === 'proposed' ? 'pending' : state === 'reverted' ? 'reverted' : 'applied' });
        if (state === 'applied') {
          record(e.item.id, field, after, { source: agent ? 'agent' : 'user', provider: null, agent, by, changeset: id, at: t0 - ago, sources: e.sources });
        }
      }
    }
    if (changes.length === 0) return;
    push(summary, state, agent, by, ago, changes);
  };

  /** A changeset with its changes as they are. */
  const push = (summary: string, state: ChangesetState, agent: string | null, by: string, ago: number, changes: Change[]) => {
    const id = journal.length ? Math.max(...journal.map((j) => j.id)) + 1 : nextId++;
    nextId = Math.max(nextId, id + 1);
    const items = new Set(changes.map((c) => c.item)).size;
    journal.push({
      id,
      state,
      summary,
      agent,
      by,
      created_at: t0 - ago,
      decided_at: state === 'proposed' ? null : t0 - ago,
      decided_by: state === 'proposed' ? null : by,
      reverted_at: state === 'reverted' ? t0 - ago + 60_000 : null,
      reverted_by: state === 'reverted' ? by : null,
      item_count: items,
      change_count: changes.length,
      moves_files: changes.some((c) => PLAN_FIELDS.has(c.field)),
      changes,
    });
    return id;
  };

  const owner = 'james@example.com';
  seed('Added a poster for Heat from the Criterion press kit', 'reverted', 'claude-code', owner, 3 * 86_400_000, [
    { item: find('Heat'), values: { 'art.poster': { pinned: 9001, added: 9001 } }, sources: [{ url: 'https://www.criterion.com/films/heat', title: 'Criterion' }] },
  ]);
  seed('Original titles, directors and languages for 3 Japanese films, from Wikipedia', 'applied', 'claude-code', owner, 2 * 3_600_000, [
    {
      item: find('Spirited Away'),
      values: { original_title: '千と千尋の神隠し', directors: ['Hayao Miyazaki'], languages: ['ja'], countries: ['JP'], studios: ['Studio Ghibli'], tags: ['Studio Ghibli'], 'ids.wikidata': 'Q155653' },
      sources: [WIKI('Spirited_Away'), { url: 'https://www.wikidata.org/wiki/Q155653', title: 'Wikidata' }],
    },
    {
      item: find('Princess Mononoke'),
      values: { original_title: 'もののけ姫', directors: ['Hayao Miyazaki'], languages: ['ja'], countries: ['JP'], studios: ['Studio Ghibli'], tags: ['Studio Ghibli'] },
      sources: [WIKI('Princess_Mononoke')],
    },
    {
      item: find('Akira'),
      values: { directors: ['Katsuhiro Otomo'], writers: ['Katsuhiro Otomo', 'Izo Hashimoto'], languages: ['ja'], countries: ['JP'], cast: [{ name: 'Mitsuo Iwata', role: 'Shōtarō Kaneda' }, { name: 'Nozomu Sasaki', role: 'Tetsuo Shima' }] },
      sources: [WIKI('Akira_(1988_film)')],
    },
  ]);
  const severance = find('Severance');
  const season = severance ? db.children(severance.id).find((c) => c.kind === 'season') : undefined;
  const episodes = season ? db.children(season.id).slice(0, 2) : [];
  seed(
    'Episode titles for Severance season 1, from the broadcaster’s episode guide',
    'proposed',
    'codex',
    'sam@example.com',
    25 * 60_000,
    episodes.map((e, i) => ({ item: e, values: { title: ['Good News About Hell', 'Half Loop'][i] }, sources: [{ url: 'https://tv.apple.com/show/severance', title: 'Apple TV+' }] })),
  );

  // A file plan an agent proposed, and a fix it applied.
  const spirited = find('Spirited Away');
  if (spirited) {
    const root = '/media/Movies';
    const step = (field: string, before: unknown, after: unknown): Change => ({
      item: spirited.id,
      title: spirited.title,
      field,
      before,
      after,
      withheld: null,
      sources: [],
      state: 'pending',
    });
    push('Name Spirited Away’s files the standard way', 'proposed', 'claude-code', owner, 12 * 60_000, [
      step('folder.path', { root: 1, path: 'sen to chihiro (2001) 1080p' }, { root: 1, path: 'Spirited Away (2001)' }),
      step('file.path', { root: 1, path: 'Spirited Away (2001)/Sen.to.Chihiro.2001.1080p.BluRay.mkv', what: 'video', file: 501 }, { root: 1, path: 'Spirited Away (2001)/Spirited Away (2001).mkv', what: 'video', file: 501 }),
      step('file.path', { root: 1, path: 'Spirited Away (2001)/Sen.to.Chihiro.2001.1080p.BluRay.ja.srt', what: 'companion', file: null }, { root: 1, path: 'Spirited Away (2001)/Spirited Away (2001).ja.srt', what: 'companion', file: null }),
    ]);
    void root;
  }
  const heat = find('Heat');
  if (heat) {
    push('Two copies of Heat are one film: the 4K joins the 1080p', 'applied', 'claude-code', owner, 50 * 60_000, [
      {
        item: heat.id,
        title: heat.title,
        field: 'file.fix',
        before: { file: 502, path: 'Heat.1995.2160p.UHD.mkv', fix: null },
        after: { file: 502, path: 'Heat.1995.2160p.UHD.mkv', fix: { kind: null, of: heat.id, title: null, year: null, season: null, episodes: null, extra: null, edition: null, part: null }, item: heat.id },
        withheld: null,
        sources: [],
        state: 'applied',
      },
    ]);
  }

  const lensSpec: LensSpec = {
    name: 'Departure board',
    blurb: 'Titles roll past on a split-flap board, the next one boarding at the gate.',
    input: 'y',
    step: 64,
    window: 30,
    animated: false,
    css: `.lens .item { width: min(86vw, 760px); display: grid; grid-template-columns: 48px 1fr auto; gap: 18px; align-items: center; padding: 10px 16px; border-radius: 10px; background: color-mix(in srgb, var(--bg) 70%, #000); font-family: var(--font-mono); color: var(--text-2); }
.lens .item .art { width: 48px; border-radius: 4px; }
.lens .item b { font-family: var(--font-cond); font-stretch: 70%; font-weight: 600; font-size: 1.375rem; letter-spacing: .02em; text-transform: uppercase; color: var(--text); }
.lens .item.is-focus { background: color-mix(in srgb, var(--bg) 40%, #000); box-shadow: 0 0 0 1px var(--line); }
.lens .item .gate { font-size: .75rem; color: var(--text-3); }`,
    scene: '',
    item: '<div class="art"></div><b>{{title}}</b><span class="gate">{{year}}</span>',
    layout: `function layout(items, view) {
  return items.map((it) => {
    const d = it.i - view.pos;
    if (Math.abs(d) > 8) return { o: 0 };
    return { y: d * 68, rx: clamp(-d * 9, -60, 60), o: 1 - Math.abs(d) * 0.1, s: d === 0 ? 1.02 : 1 };
  });
}`,
  };
  lenses.push({ id: 's-departure-board', everyone: false, added_by: owner, agent: 'claude-code', added_at: t0 - 86_400_000, updated_at: t0 - 86_400_000, spec: lensSpec });

  const brief = (s: ItemSummary): AgentItem => ({
    id: s.id,
    kind: s.kind,
    library: 1,
    parent: null,
    title: s.title,
    year: s.year,
    subtitle: s.subtitle,
    match_state: 'matched',
    gaps: [],
    poster: s.poster?.source ?? null,
    available: s.available,
    path: null,
  });

  const itemGet = (id: Id): AgentItemDetail | null => {
    const d = db.detail(id);
    const rec = db.get(id);
    if (!d || !rec) return null;
    const provider = MATCHED_IDS[`${rec.title}|${rec.year}`] !== undefined;
    const values: Values = { title: d.title, year: d.year, overview: d.overview, genres: d.genres.length ? d.genres : null };
    for (const [p, v] of d.external_ids) values[`ids.${p}`] = v;
    const sources: Record<string, FieldSource> = {};
    if (provider) {
      for (const f of ['title', 'overview', 'genres', 'ids.tmdb']) {
        if (values[f] !== null && values[f] !== undefined) sources[f] = { source: 'provider', provider: 'tmdb', agent: null, by: null, changeset: null, at: null, sources: [] };
      }
    }
    const mine = set.get(id);
    if (mine) {
      Object.assign(values, mine.values);
      Object.assign(sources, mine.sources);
    }
    const fields: Fields = { ...EMPTY_FIELDS };
    const ids: Record<string, string> = {};
    for (const [k, v] of Object.entries(values)) {
      if (v === null || v === undefined) continue;
      if (k.startsWith('ids.')) ids[k.slice(4)] = typeof v === 'string' ? v : JSON.stringify(v);
      else if (k in fields) (fields as unknown as Record<string, unknown>)[k] = v;
    }
    fields.ids = Object.keys(ids).length ? ids : null;
    const files: FileEvidence[] = d.assets.map((a) => ({
      id: a.id,
      path: a.path.replace(/^\/media\/[^/]+\//, ''),
      root: a.path.match(/^\/media\/[^/]+/)?.[0] ?? '/media',
      sidecars: [],
      fix: null,
      size: a.size,
      modified: null,
      available: a.available,
      container: a.container,
      duration_ms: a.duration_ms,
      container_title: null,
      edition: a.edition,
      tracks: (a.probe?.tracks ?? [])
        .filter((t) => t.kind === 'video' || t.kind === 'audio' || t.kind === 'subtitle')
        .map((t) => ({ kind: t.kind, codec: t.codec, language: t.language, title: t.title, width: t.width, height: t.height, fps: null, channels: t.channels, hdr: null, default: t.default, forced: t.forced })),
      chapters: [],
      subtitle_files: a.subtitle_files.map((f) => f.path),
      parsed: null,
    }));
    const art: AgentArt[] = [];
    for (const [kind, ref] of [
      ['poster', d.poster],
      ['backdrop', d.backdrop],
    ] as const) {
      if (ref) art.push({ id: 0, kind, source: ref.source, shown: true, pinned: false, width: ref.width, height: ref.height, credit: ref.attribution, found_at: null, generator: ref.generator, url: ref.url, withheld: null });
    }
    return {
      ...brief(d),
      path: d.assets[0]?.path ?? null,
      fields,
      sources,
      withheld: {},
      match_confidence: d.match_confidence,
      season_number: d.season_number,
      episode_numbers: d.episode_numbers,
      files,
      folders: [],
      art,
      children: db.children(id).map(brief),
      changes: journal.filter((c) => c.changes.some((ch) => ch.item === id)).map(summaryOf).reverse().slice(0, 10),
    };
  };

  const summaryOf = (c: Changeset) => ({
    id: c.id,
    state: c.state,
    summary: c.summary,
    agent: c.agent,
    by: c.by,
    created_at: c.created_at,
    decided_at: c.decided_at,
    decided_by: c.decided_by,
    reverted_at: c.reverted_at,
    reverted_by: c.reverted_by,
    item_count: c.item_count,
    change_count: c.change_count,
    moves_files: c.moves_files,
  });

  const announce = (c: Changeset) => {
    ctx.broadcast({ type: 'changes', changeset: summaryOf(c) });
    const byLib = new Map<Id, Id[]>();
    for (const ch of c.changes) {
      const lib = db.summary(ch.item) ? 1 : null;
      if (lib) byLib.set(lib, [...(byLib.get(lib) ?? []), ch.item]);
    }
    for (const [library_id, updated] of byLib) ctx.broadcast({ type: 'items_changed', library_id, added: [], updated, removed: [] });
  };

  const decide = (id: Id, action: 'undo' | 'apply' | 'reject'): Decided | Response => {
    const c = journal.find((j) => j.id === id);
    if (!c) return opError(404, 'not_found', `No changeset ${id}`);
    const want = action === 'undo' ? 'applied' : 'proposed';
    if (c.state !== want) return opError(409, 'conflict', `changeset ${id} is ${c.state}${action === 'undo' ? ': only an applied one can be undone' : ', not proposed'}`);
    const notDone: NotDone[] = [];
    let done = 0;
    const now = Date.now();
    for (const ch of [...c.changes].reverse()) {
      // The mock moves no files and places nothing again: a step is done.
      if (PLAN_FIELDS.has(ch.field) || ch.field === 'file.fix') {
        if (action === 'undo' && ch.state === 'applied') {
          ch.state = 'reverted';
          done += 1;
        } else if (action === 'apply' && ch.state === 'pending') {
          ch.state = 'applied';
          done += 1;
        }
        continue;
      }
      if (action === 'undo' && ch.state === 'applied') {
        if (JSON.stringify(valueOf(ch.item, ch.field)) === JSON.stringify(ch.after)) {
          record(ch.item, ch.field, ch.before, null);
          ch.state = 'reverted';
          done += 1;
        } else {
          ch.state = 'kept';
          notDone.push({ item: ch.item, field: ch.field, reason: 'it changed again since' });
        }
      } else if (action === 'apply' && ch.state === 'pending') {
        record(ch.item, ch.field, ch.after, { source: c.agent ? 'agent' : 'user', provider: null, agent: c.agent, by: c.by, changeset: c.id, at: now, sources: ch.sources });
        ch.state = 'applied';
        done += 1;
      }
    }
    if (action === 'undo') {
      c.state = 'reverted';
      c.reverted_at = now;
      c.reverted_by = owner;
    } else {
      c.state = action === 'apply' ? 'applied' : 'rejected';
      c.decided_at = now;
      c.decided_by = owner;
    }
    announce(c);
    return { changeset: summaryOf(c), done, not_done: notDone };
  };

  interface MockCollection {
    id: Id;
    name: string;
    description: string | null;
    everyone: boolean;
    rule: CollectionRule | null;
    picks: { item: Id; note: string | null }[];
    agent: string | null;
    at: number;
  }
  const collections: MockCollection[] = [];
  const ghibli = ['Spirited Away', 'Princess Mononoke', 'My Neighbor Totoro', 'Howl’s Moving Castle', 'Kiki’s Delivery Service']
    .map((t) => find(t))
    .filter((x): x is ItemSummary => !!x);
  if (ghibli.length) {
    collections.push({
      id: 1,
      name: 'Ghibli',
      description: 'Studio Ghibli’s features, in the order they came out.',
      everyone: true,
      rule: null,
      picks: ghibli.map((g, i) => ({ item: g.id, note: i === 0 ? 'The one to start with' : null })),
      agent: 'claude-code',
      at: t0 - 3_600_000,
    });
  }
  const rainy = db
    .libraryIds(1, 'year')
    .map((i) => db.summary(i))
    .filter((x): x is ItemSummary => !!x && x.kind === 'movie' && (x.year ?? 9999) < 1990)
    .slice(0, 8);
  if (rainy.length) {
    collections.push({ id: 2, name: 'Rainy Sunday', description: 'Old favourites for a slow afternoon.', everyone: false, rule: null, picks: rainy.map((r) => ({ item: r.id, note: null })), agent: 'claude-code', at: t0 - 7_200_000 });
  }
  const membersOf = (c: MockCollection): Id[] => c.picks.map((p) => p.item).filter((i) => db.summary(i));
  const infoOf = (c: MockCollection): CollectionInfo => {
    const ids = membersOf(c);
    return {
      id: c.id,
      name: c.name,
      description: c.description,
      everyone: c.everyone,
      rule: c.rule,
      count: ids.length,
      cover: ids.slice(0, 4),
      added_by: owner,
      agent: c.agent,
      sources: [],
      created_at: c.at,
      updated_at: c.at,
    };
  };

  const ops: Record<string, (input: Record<string, unknown>) => unknown> = {
    'changes.list': (input) => {
      const state = input.state as ChangesetState | undefined;
      const item = input.item as Id | undefined;
      const before = input.before as Id | undefined;
      const limit = Math.min(200, Math.max(1, Number(input.limit ?? 50)));
      const all = [...journal]
        .reverse()
        .filter((c) => (!state || c.state === state) && (!item || c.changes.some((ch) => ch.item === item)) && (!before || c.id < before));
      const page = all.slice(0, limit);
      const out: ChangesListOutput = {
        changesets: page.map(summaryOf),
        next: all.length > limit ? (page[page.length - 1]?.id ?? null) : null,
        proposed: journal.filter((c) => c.state === 'proposed').length,
      };
      return out;
    },
    'changes.get': (input) => {
      const c = journal.find((j) => j.id === input.changeset);
      return c ? { ...c, changes: c.changes.map((ch) => ({ ...ch, title: db.summary(ch.item)?.title ?? ch.title })) } : opError(404, 'not_found', `No changeset ${String(input.changeset)}`);
    },
    'changes.undo': (input) => decide(Number(input.changeset), 'undo'),
    'changes.apply': (input) => decide(Number(input.changeset), 'apply'),
    'changes.reject': (input) => decide(Number(input.changeset), 'reject'),
    'item.get': (input) => itemGet(Number(input.item)) ?? opError(404, 'not_found', `No item ${String(input.item)}`),
    'items.edit': (input) => {
      const edits = (input.edits as { item: Id; set?: Record<string, unknown>; clear?: string[] }[] | undefined) ?? [];
      const summary = typeof input.summary === 'string' ? input.summary.trim() : '';
      if (!summary) return opError(400, 'invalid', 'Give a summary: what these changes are and why');
      const propose = input.mode === 'propose';
      const changes: Change[] = [];
      const results: EditOutput['items'] = [];
      for (const e of edits) {
        const s = db.summary(e.item);
        if (!s) return opError(404, 'not_found', `No item ${e.item}`);
        const changed: EditOutput['items'][number]['changed'] = [];
        const flat: Record<string, unknown> = {};
        for (const [k, v] of Object.entries(e.set ?? {})) {
          if (v === null || v === undefined) continue;
          if (k === 'ids' && typeof v === 'object') for (const [p, id] of Object.entries(v as Record<string, unknown>)) flat[`ids.${p}`] = id;
          else flat[k] = v;
        }
        for (const f of e.clear ?? []) flat[f] = null;
        for (const [field, after] of Object.entries(flat)) {
          const before = valueOf(e.item, field) ?? (field === 'title' ? s.title : field === 'year' ? s.year : null);
          if (JSON.stringify(before) === JSON.stringify(after)) continue;
          changes.push({ item: e.item, title: s.title, field, before, after, withheld: null, sources: [], state: propose ? 'pending' : 'applied' });
          changed.push({ field, before, after, withheld: null });
        }
        results.push({ item: e.item, title: s.title, changed, unchanged: [], refused: [] });
      }
      if (changes.length === 0) return { changeset: null, items: results } satisfies EditOutput;
      const id = push(summary, propose ? 'proposed' : 'applied', null, owner, 0, changes);
      const c = journal.find((j) => j.id === id)!;
      c.created_at = Date.now();
      if (!propose) {
        for (const ch of changes) record(ch.item, ch.field, ch.after, ch.after === null ? null : { source: 'user', provider: null, agent: null, by: owner, changeset: id, at: Date.now(), sources: [] });
      }
      announce(c);
      return { changeset: summaryOf(c), items: results } satisfies EditOutput;
    },
    'collections.list': (): Collections => ({ collections: collections.map(infoOf) }),
    'collection.get': (input) => {
      const c = collections.find((x) => x.id === input.collection);
      if (!c) return opError(404, 'not_found', `No collection ${String(input.collection)}`);
      const ids = membersOf(c);
      const out: CollectionItems = {
        collection: infoOf(c),
        items: ids.map((i) => db.summary(i)).filter((x): x is ItemSummary => !!x).map(brief),
        notes: Object.fromEntries(c.picks.filter((p) => p.note).map((p) => [String(p.item), p.note as string])),
        next: null,
        total: ids.length,
      };
      return out;
    },
    'collection.put': (input) => {
      const name = typeof input.name === 'string' ? input.name.trim() : '';
      if (!name) return opError(400, 'invalid', 'Give a name of 1 to 60 characters');
      let c = collections.find((x) => x.id === input.id || x.name.toLowerCase() === name.toLowerCase());
      if (!c) {
        c = { id: collections.length + 1, name, description: null, everyone: Boolean(input.everyone), rule: null, picks: [], agent: null, at: Date.now() };
        collections.push(c);
      }
      c.name = name;
      if (typeof input.description === 'string') c.description = input.description;
      if (Array.isArray(input.items)) c.picks = (input.items as CollectionPick[]).map((p) => ({ item: p.item, note: p.note ?? null }));
      for (const p of (input.add as CollectionPick[] | undefined) ?? []) if (!c.picks.some((x) => x.item === p.item)) c.picks.push({ item: p.item, note: p.note ?? null });
      const remove = (input.remove as Id[] | undefined) ?? [];
      c.picks = c.picks.filter((p) => !remove.includes(p.item));
      if (input.rule !== undefined) c.rule = (input.rule as CollectionRule | null) ?? null;
      ctx.broadcast({ type: 'collections' });
      return infoOf(c);
    },
    'collection.remove': (input) => {
      const i = collections.findIndex((x) => x.id === input.collection);
      if (i >= 0) {
        collections.splice(i, 1);
        ctx.broadcast({ type: 'collections' });
      }
      return { removed: i >= 0 };
    },
    'lenses.list': (): Lenses => ({ lenses }),
    'lens.remove': (input) => {
      const i = lenses.findIndex((l) => l.id === input.id);
      if (i >= 0) {
        lenses.splice(i, 1);
        ctx.broadcast({ type: 'lenses' });
      }
      return { removed: i >= 0 };
    },
  };

  const manifestOf = (): Manifest => ({
    contract: 'flux.agent/1',
    server: { name: 'flux (mock)', version: '0.1.0' },
    ops: Object.keys(ops).map((name) => ({ name, title: name, description: '', effect: name.startsWith('changes.') && name !== 'changes.list' && name !== 'changes.get' ? 'write' : 'read', who: 'anyone', idempotent: true, destructive: false, input: {}, output: {} })),
    mcp: '/mcp',
    skills: [
      { name: 'flux', description: 'Work with a flux media server.', url: '/api/v1/agent/skills/flux' },
      { name: 'flux-metadata', description: 'Identify titles and fill in what’s missing from the open web, with sources.', url: '/api/v1/agent/skills/flux-metadata' },
      { name: 'flux-artwork', description: 'Find, check and credit posters and backdrops.', url: '/api/v1/agent/skills/flux-artwork' },
      { name: 'flux-lens', description: 'Design a new way of browsing the library.', url: '/api/v1/agent/skills/flux-lens' },
    ],
    schemas: [],
    policy: { withheld: ['tmdb'], rules: [] },
  });

  return [
    http.get(u('/api/v1/collections/:id/items'), async ({ params, request }) => {
      await wait();
      const c = collections.find((x) => x.id === Number(params.id));
      if (!c) return new HttpResponse(`No collection ${String(params.id)}`, { status: 404 });
      const url = new URL(request.url);
      const sort = (url.searchParams.get('sort') ?? 'title') as LibrarySort;
      const items = membersOf(c)
        .map((i) => db.summary(i))
        .filter((x): x is ItemSummary => !!x);
      const by: Record<LibrarySort, (a: ItemSummary, b: ItemSummary) => number> = {
        title: (a, b) => a.title.localeCompare(b.title),
        '-title': (a, b) => b.title.localeCompare(a.title),
        year: (a, b) => (a.year ?? 9999) - (b.year ?? 9999),
        '-year': (a, b) => (b.year ?? -1) - (a.year ?? -1),
        added: (a, b) => a.added_at - b.added_at,
        '-added': (a, b) => b.added_at - a.added_at,
      };
      items.sort(by[sort] ?? by.title);
      const page: Page<ItemSummary> = { items, next: null, total: items.length };
      return HttpResponse.json(page);
    }),
    http.get(u('/api/v1/agent'), async () => {
      await wait();
      return HttpResponse.json(manifestOf());
    }),
    http.post(u('/api/v1/ops/:name'), async ({ params, request }) => {
      await wait();
      const name = String(params.name);
      const run = ops[name];
      if (!run) return opError(404, 'not_found', `The mock server doesn’t run ${name}`);
      let input: Record<string, unknown>;
      try {
        const text = await request.text();
        input = text.trim() ? (JSON.parse(text) as Record<string, unknown>) : {};
      } catch {
        return opError(400, 'invalid', 'The body isn’t JSON');
      }
      const out = run(input);
      return out instanceof Response ? out : HttpResponse.json(out as Record<string, unknown>);
    }),
  ];
}
