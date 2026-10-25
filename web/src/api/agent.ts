/**
 * The agent contract (`flux.agent/1`, flux-core `agent.rs`): the
 * operations agents use, which the web client uses too (docs/AGENT-NATIVE.md,
 * "One contract, three ways in"). Hand-mirrored like types.ts and checked
 * against real serde output by the golden test (scripts/gen-golden.mjs).
 *
 * `op(name, input)` runs one: `POST /api/v1/ops/{name}`. A person's page
 * sends no agent name, so the server withholds nothing from it.
 */
import { API_BASE } from '../config';
import { FLUX_REQUEST_HEADER, FluxApiError, whenReady } from './client';
import type { ArtSource, Id, ItemKind, LibraryKind, NameGuess } from './types';

export const GAPS = ['identity', 'year', 'overview', 'poster', 'backdrop', 'episode_titles', 'offline'] as const;
export type Gap = (typeof GAPS)[number];
export const MATCH_STATES = ['unmatched', 'parsed', 'matched', 'locked'] as const;
export type MatchState = (typeof MATCH_STATES)[number];
export const SOURCES = ['file', 'provider', 'user', 'agent'] as const;
/** Where a value came from. */
export type Source = (typeof SOURCES)[number];
export const CHANGESET_STATES = ['proposed', 'applied', 'reverted', 'rejected'] as const;
export type ChangesetState = (typeof CHANGESET_STATES)[number];
export const CHANGE_STATES = ['pending', 'applied', 'skipped', 'reverted', 'kept'] as const;
export type ChangeState = (typeof CHANGE_STATES)[number];
export const ART_KINDS = ['poster', 'backdrop', 'thumb', 'logo'] as const;
export type ArtKind = (typeof ART_KINDS)[number];
export const ART_ORIGINS = ['found', 'made', 'generated'] as const;
export type ArtOrigin = (typeof ART_ORIGINS)[number];
export const EDIT_MODES = ['apply', 'propose'] as const;
export type EditMode = (typeof EDIT_MODES)[number];
export const ERROR_CODES = ['invalid', 'not_found', 'forbidden', 'conflict', 'unavailable', 'too_large', 'internal'] as const;
export type ErrorCode = (typeof ERROR_CODES)[number];
export const LENS_INPUTS = ['x', 'y', 'none'] as const;
export type LensInput = (typeof LENS_INPUTS)[number];
export const EFFECTS = ['read', 'write', 'act'] as const;
export type Effect = (typeof EFFECTS)[number];

export interface Link {
  url: string;
  title: string | null;
}

export interface CastMember {
  name: string;
  role: string | null;
}

/** The facts people and agents set. */
export interface Fields {
  title: string | null;
  original_title: string | null;
  year: number | null;
  overview: string | null;
  tagline: string | null;
  genres: string[] | null;
  tags: string[] | null;
  release_date: string | null;
  air_date: string | null;
  languages: string[] | null;
  countries: string[] | null;
  directors: string[] | null;
  writers: string[] | null;
  cast: CastMember[] | null;
  studios: string[] | null;
  content_rating: string | null;
  links: Link[] | null;
  ids: Record<string, string> | null;
}

/** Who set a field, when, and from what. */
export interface FieldSource {
  source: Source;
  provider: string | null;
  agent: string | null;
  by: string | null;
  changeset: Id | null;
  at: number | null;
  sources: Link[];
}

export interface AgentItem {
  id: Id;
  kind: ItemKind;
  library: Id;
  parent: Id | null;
  title: string;
  year: number | null;
  subtitle: string | null;
  match_state: MatchState;
  gaps: Gap[];
  poster: ArtSource | null;
  available: boolean;
  path: string | null;
}

export interface TrackEvidence {
  kind: string;
  codec: string;
  language: string | null;
  title: string | null;
  width: number | null;
  height: number | null;
  fps: number | null;
  channels: number | null;
  hdr: string | null;
  default: boolean;
  forced: boolean;
}

export interface FileEvidence {
  path: string;
  size: number;
  modified: number | null;
  available: boolean;
  container: string | null;
  duration_ms: number | null;
  container_title: string | null;
  edition: string | null;
  tracks: TrackEvidence[];
  chapters: string[];
  subtitle_files: string[];
  parsed: NameGuess | null;
}

export interface AgentArt {
  id: Id;
  kind: ArtKind;
  source: ArtSource;
  shown: boolean;
  pinned: boolean;
  width: number | null;
  height: number | null;
  credit: string | null;
  found_at: string | null;
  generator: string | null;
  url: string | null;
  withheld: string | null;
}

/** `item.get`. */
export interface AgentItemDetail extends AgentItem {
  fields: Fields;
  sources: Record<string, FieldSource>;
  withheld: Record<string, string>;
  match_confidence: number | null;
  season_number: number | null;
  episode_numbers: number[];
  files: FileEvidence[];
  folders: string[];
  art: AgentArt[];
  children: AgentItem[];
  changes: ChangesetSummary[];
}

export interface ChangesetSummary {
  id: Id;
  state: ChangesetState;
  summary: string;
  agent: string | null;
  by: string;
  created_at: number;
  decided_at: number | null;
  decided_by: string | null;
  reverted_at: number | null;
  reverted_by: string | null;
  item_count: number;
  change_count: number;
}

export interface Change {
  item: Id;
  title: string | null;
  /** A field name, or `art.<kind>`. */
  field: string;
  before: unknown;
  after: unknown;
  withheld: string | null;
  sources: Link[];
  state: ChangeState;
}

/** `changes.get`. */
export interface Changeset extends ChangesetSummary {
  changes: Change[];
}

/** `changes.list`. */
export interface ChangesListOutput {
  changesets: ChangesetSummary[];
  next: Id | null;
  proposed: number;
}

export interface NotDone {
  item: Id;
  field: string;
  reason: string;
}

/** `changes.undo`, `changes.apply`, `changes.reject`. */
export interface Decided {
  changeset: ChangesetSummary;
  done: number;
  not_done: NotDone[];
}

export interface LensSpec {
  name: string;
  blurb: string;
  input: LensInput;
  step: number;
  window: number;
  animated: boolean;
  css: string;
  scene: string;
  item: string;
  layout: string;
}

export interface StoredLens {
  id: string;
  everyone: boolean;
  added_by: string;
  agent: string | null;
  added_at: number;
  updated_at: number;
  spec: LensSpec;
}

/** `lenses.list`. */
export interface Lenses {
  lenses: StoredLens[];
}

export interface OpError {
  code: ErrorCode;
  message: string;
}

export interface ErrorBody {
  error: OpError;
}

export interface OpInfo {
  name: string;
  title: string;
  description: string;
  effect: Effect;
  who: string;
  idempotent: boolean;
  destructive: boolean;
  input: unknown;
  output: unknown;
}

export interface SkillInfo {
  name: string;
  description: string;
  url: string;
}

export interface Policy {
  withheld: string[];
  rules: string[];
}

/** `GET /api/v1/agent`. */
export interface Manifest {
  contract: string;
  server: { name: string; version: string };
  ops: OpInfo[];
  mcp: string;
  skills: SkillInfo[];
  schemas: string[];
  policy: Policy;
}

/** An operation's failure, with the server's code. */
export class OpFailure extends FluxApiError {
  readonly code: ErrorCode | 'error';

  constructor(status: number, path: string, code: ErrorCode | 'error', message: string) {
    super(status, path, message);
    this.name = 'OpFailure';
    this.code = code;
  }
}

/** Run operation `name` (`POST /api/v1/ops/{name}`). */
export async function op<T>(name: string, input: unknown = {}): Promise<T> {
  await whenReady();
  const path = `/api/v1/ops/${name}`;
  const res = await fetch(API_BASE.replace(/\/+$/, '') + path, {
    method: 'POST',
    headers: { accept: 'application/json', 'content-type': 'application/json', [FLUX_REQUEST_HEADER]: '1' },
    body: JSON.stringify(input ?? {}),
  });
  const text = await res.text();
  if (!res.ok) {
    let code: ErrorCode | 'error' = 'error';
    let message = text.trim() || `${name} failed with HTTP ${res.status}`;
    try {
      const body = JSON.parse(text) as Partial<ErrorBody>;
      if (body.error) {
        code = body.error.code;
        message = body.error.message;
      }
    } catch {
      // a plain-text refusal (the request guard)
    }
    throw new OpFailure(res.status, path, code, message);
  }
  return JSON.parse(text) as T;
}

/** `GET /api/v1/agent`. */
export async function manifest(): Promise<Manifest> {
  await whenReady();
  const res = await fetch(`${API_BASE.replace(/\/+$/, '')}/api/v1/agent`, { headers: { [FLUX_REQUEST_HEADER]: '1' } });
  if (!res.ok) throw new FluxApiError(res.status, '/api/v1/agent', `HTTP ${res.status}`);
  return (await res.json()) as Manifest;
}

/** The libraries' kinds an agent can't match (`videos`). */
export const UNMATCHED_LIBRARY_KINDS: readonly LibraryKind[] = ['videos'];

/** Field names as people read them. */
export const FIELD_NAMES: Record<string, string> = {
  title: 'Title',
  original_title: 'Original title',
  year: 'Year',
  overview: 'Description',
  tagline: 'Tagline',
  genres: 'Genres',
  tags: 'Tags',
  release_date: 'Released',
  air_date: 'Aired',
  languages: 'Languages',
  countries: 'Countries',
  directors: 'Directed by',
  writers: 'Written by',
  cast: 'Cast',
  studios: 'Studios',
  content_rating: 'Rating',
  links: 'Links',
  'art.poster': 'Poster',
  'art.backdrop': 'Backdrop',
  'art.thumb': 'Thumbnail',
  'art.logo': 'Logo',
};

/** `ids.tmdb` → "TMDB id", and every other field by its name. */
export function fieldName(field: string): string {
  if (field.startsWith('ids.')) {
    const p = field.slice(4);
    const known: Record<string, string> = { tmdb: 'TMDB', imdb: 'IMDb', tvdb: 'TheTVDB', wikidata: 'Wikidata', anidb: 'AniDB' };
    return `${known[p] ?? p} id`;
  }
  return FIELD_NAMES[field] ?? field.replace(/_/g, ' ');
}

const languageNames = (() => {
  try {
    return new Intl.DisplayNames(['en'], { type: 'language' });
  } catch {
    return null;
  }
})();
const regionNames = (() => {
  try {
    return new Intl.DisplayNames(['en'], { type: 'region' });
  } catch {
    return null;
  }
})();

/** Countries that are no more, which Intl doesn't name. */
const FORMER_COUNTRIES: Record<string, string> = { SU: 'Soviet Union', YU: 'Yugoslavia', DD: 'East Germany', CS: 'Czechoslovakia' };

/** `ja` → "Japanese" (the code itself when Intl doesn't know it). */
export function languageName(code: string): string {
  try {
    return languageNames?.of(code) ?? code;
  } catch {
    return code;
  }
}

/** `JP` → "Japan", `SU` → "Soviet Union". */
export function countryName(code: string): string {
  if (FORMER_COUNTRIES[code]) return FORMER_COUNTRIES[code];
  try {
    const n = regionNames?.of(code);
    return n && n !== code ? n : code;
  } catch {
    return code;
  }
}

/** A value as one line of text; `field` names codes (`languages`, `countries`). */
export function valueText(v: unknown, field?: string): string {
  if (Array.isArray(v) && (field === 'languages' || field === 'countries')) {
    const name = field === 'languages' ? languageName : countryName;
    return v.map((c) => (typeof c === 'string' ? name(c) : JSON.stringify(c))).join(', ');
  }
  if (v === null || v === undefined) return '—';
  if (typeof v === 'string') return v;
  if (typeof v === 'number' || typeof v === 'boolean') return String(v);
  if (Array.isArray(v)) {
    return v
      .map((x: unknown) => {
        if (typeof x === 'string' || typeof x === 'number') return String(x);
        if (x && typeof x === 'object') {
          const o = x as Record<string, unknown>;
          if (typeof o.name === 'string') return typeof o.role === 'string' && o.role ? `${o.name} (${o.role})` : o.name;
          if (typeof o.url === 'string') return typeof o.title === 'string' && o.title ? o.title : hostOf(o.url);
        }
        return JSON.stringify(x);
      })
      .join(', ');
  }
  if (typeof v === 'object') {
    const o = v as Record<string, unknown>;
    if ('pinned' in o) return typeof o.pinned === 'number' ? `image ${o.pinned}` : 'flux’s choice';
  }
  return JSON.stringify(v);
}

/** `https://en.wikipedia.org/wiki/X` → `en.wikipedia.org`. */
export function hostOf(url: string): string {
  try {
    return new URL(url).host.replace(/^www\./, '');
  } catch {
    return url;
  }
}
