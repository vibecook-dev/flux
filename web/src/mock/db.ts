/**
 * In-memory catalog behind the mock backend, built from fixtures.json (the
 * synthetic test library's ground truth + the feature clips' probe facts).
 *
 * Three libraries:
 *  1. Movies       2,998 titles from library.manifest.jsonl (real + synthetic names)
 *  2. TV Shows     300 shows, seasons and ~11k episodes from the same manifest
 *  3. Film Archive 10,000 generated titles, to exercise the virtualised grid at
 *                  10k; one of its two roots is "unavailable" (asleep NAS).
 *
 * Everything is deterministic (hash-seeded) and summaries are built lazily.
 */
import { foldForSearch, titleSortKey } from '../api/format';
import type {
  ArtRef,
  ArtSource,
  AssetInfo,
  HdrFormat,
  Home,
  Id,
  ItemDetail,
  ItemKind,
  ItemFilter,
  ItemSummary,
  Library,
  LibrarySort,
  ProbeResult,
  Progress,
  RootStatus,
  ScanStatus,
  Track,
} from '../api/types';
import { artBlurhash, hash32, rng } from './art';
import { CURATED, MATCHED_IDS } from './curated';

type FileFix = [path: string, seed: string, edition: string | null, part: number | null, resolution: string | null];

export interface Fixtures {
  seed_profiles: Record<string, string[]>;
  profiles: Record<string, { file: string; bytes: number; probe: ProbeResult }>;
  movies: [title: string, year: number | null, tmdb: number | null, files: FileFix[]][];
  shows: [name: string, year: number | null, samplePath: string, seed: string, seasons: Record<string, number[]>][];
}

export const LIBRARY_ROOT = '/Volumes/Media/library';
const ARCHIVE_ROOTS = ['/Volumes/Archive/Films', '/Volumes/NAS/Films'];
const DAY = 86_400_000;

const GENRES = [
  'Drama', 'Science Fiction', 'Thriller', 'Comedy', 'Animation', 'Crime', 'Romance', 'Adventure', 'Horror',
  'Documentary', 'Mystery', 'Fantasy', 'War', 'Western', 'Music',
];
const ADJ = ['quiet', 'restless', 'sunlit', 'haunted', 'stubborn', 'tender', 'feverish', 'unhurried', 'bitter', 'luminous'];
const NOUN = ['harbour town', 'mountain pass', 'border city', 'family farm', 'night shift', 'river delta', 'radio station', 'island ferry'];
const ARCH_A = ['Silver', 'Winter', 'Amber', 'Hidden', 'Scarlet', 'Pale', 'Distant', 'Second', 'Western', 'Little', 'Open', 'Cold', 'Long', 'High', 'Low', 'Black', 'White', 'Blue', 'Late', 'Early'];
const ARCH_B = ['River', 'Station', 'Letters', 'Summer', 'Shore', 'Window', 'Season', 'Road', 'Garden', 'Fever', 'Light', 'Hours', 'Tide', 'Bridge', 'Valley', 'House', 'Weather', 'Crossing', 'Signal', 'Morning'];
const ARCH_C = ['', '', '', ' of the North', ' in August', ' at Dusk', ' Revisited', ' No. 2', ' for Two', ' Without End'];

/** One playable file, before it is turned into an AssetInfo. */
interface FileRec {
  path: string;
  profile: string;
  edition: string | null;
  part: number | null;
  resolution: string | null;
  available: boolean;
}

export interface ItemRec {
  id: Id;
  kind: ItemKind;
  libraryId: Id;
  title: string;
  year: number | null;
  tmdb: number | null;
  files: FileRec[];
  addedAt: number;
  runtimeMs: number;
  posterSource: ArtSource | null;
  progress: Progress | null;
  // TV structure
  showId: Id | null;
  season: number | null;
  episodes: number[];
  children: Id[];
}

export interface MockDb {
  libraries: Library[];
  rootAvailable: Map<string, boolean>;
  get(id: Id): ItemRec | undefined;
  summary(id: Id): ItemSummary | undefined;
  detail(id: Id): ItemDetail | undefined;
  children(id: Id): ItemSummary[];
  libraryIds(libraryId: Id, sort: LibrarySort): Id[];
  /** `ItemFilter` semantics of flux-catalog: shows match through their episodes. */
  matchesFilter(id: Id, filter: ItemFilter): boolean;
  home(): Home;
  search(q: string): ItemSummary[];
  setProgress(id: Id, positionMs: number, durationMs: number): void;
  /** `POST /items/{id}/watched`: a show or season marks every episode. */
  setWatched(id: Id, watched: boolean): void;
  /** Add items to a library (used by the scan simulation for new libraries). */
  addItems(libraryId: Id, count: number): Id[];
  createLibrary(name: string, kind: Library['kind'], roots: string[]): Library;
  /** Feature clip standing in for file `index` of item `id`, with its real length (mock playback). */
  profileFile(id: Id, index: number): { file: string; durationMs: number | null } | undefined;
  now: number;
}

function resolutionOf(res: string | null, probe: ProbeResult): [number, number] | null {
  switch (res) {
    case '2160p':
      return [3840, 2160];
    case '1080p':
      return [1920, 1080];
    case '720p':
      return [1280, 720];
    case '480p':
      return [854, 480];
    default: {
      const v = probe.tracks.find((t) => t.kind === 'video');
      return v?.width && v.height ? [v.width, v.height] : null;
    }
  }
}

function resName(w: number): string {
  if (w >= 3800) return '4K';
  if (w >= 1900) return '1080p';
  if (w >= 1270) return '720p';
  if (w >= 850) return '480p';
  return 'SD';
}

const VIDEO_SHORT: Partial<Record<Track['codec'], string>> = { h264: 'H.264', hevc: 'HEVC', av1: 'AV1', mpeg2_video: 'MPEG-2', mpeg4_part2: 'MPEG-4', vp9: 'VP9' };
const AUDIO_SHORT: Partial<Record<Track['codec'], string>> = {
  aac: 'AAC', ac3: 'AC-3', eac3: 'E-AC-3', true_hd: 'TrueHD', dts: 'DTS', flac: 'FLAC', opus: 'Opus', mp3: 'MP3',
};
const HDR_SHORT: Partial<Record<HdrFormat, string>> = { hdr10: 'HDR10', hdr10_plus: 'HDR10+', hlg: 'HLG', dolby_vision: 'DV' };

function layout(ch: number | null): string {
  return ch === 8 ? '7.1' : ch === 6 ? '5.1' : ch === 2 ? '2.0' : ch === 1 ? '1.0' : '';
}

/** Badges for the best available file: "4K", "HDR10", "Atmos", "7.1". */
function badgesFor(probe: ProbeResult | null): string[] {
  if (!probe) return [];
  const v = probe.tracks.find((t) => t.kind === 'video');
  const a = probe.tracks.find((t) => t.kind === 'audio' && t.default) ?? probe.tracks.find((t) => t.kind === 'audio');
  const out: string[] = [];
  if (v?.width && v.width >= 3800) out.push('4K');
  const hdr = v?.hdr?.format;
  if (hdr && HDR_SHORT[hdr]) out.push(HDR_SHORT[hdr]);
  if (a?.title?.includes('Atmos')) out.push('Atmos');
  if (a?.channels === 8) out.push('7.1');
  else if (a?.channels === 6) out.push('5.1');
  return out;
}

export function createMockDb(fx: Fixtures, now = Date.now()): MockDb {
  const items = new Map<Id, ItemRec>();
  const byLibrary = new Map<Id, Id[]>();
  const rootAvailable = new Map<string, boolean>([
    [`${LIBRARY_ROOT}/Movies`, true],
    [`${LIBRARY_ROOT}/TV`, true],
    [ARCHIVE_ROOTS[0], true],
    [ARCHIVE_ROOTS[1], false],
  ]);
  const profileKeys = Object.keys(fx.profiles);
  const pickProfile = (seed: string, key: string) => {
    const options = fx.seed_profiles[seed] ?? profileKeys;
    return options[hash32(key) % options.length];
  };
  const add = (rec: ItemRec) => {
    items.set(rec.id, rec);
    if (rec.kind === 'movie' || rec.kind === 'show') {
      let list = byLibrary.get(rec.libraryId);
      if (!list) byLibrary.set(rec.libraryId, (list = []));
      list.push(rec.id);
    }
  };
  /** Skewed so most of the library is old and a handful arrived this week. */
  const addedAgo = (key: string) => {
    const r = rng(hash32(`added:${key}`))();
    return now - Math.floor(Math.pow(r, 2.4) * 900 * DAY) - (hash32(key) % 3600) * 1000;
  };
  const posterSourceFor = (key: string): ArtSource | null => {
    const r = hash32(`art:${key}`) % 100;
    if (r < 3) return null;
    if (r < 9) return 'frame';
    if (r < 12) return 'generated';
    if (r < 14) return 'local';
    return 'provider';
  };

  // 1. Movies.
  fx.movies.forEach(([title, year, tmdbFromPath, files], i) => {
    const id = 10_000 + i;
    const key = `${title}|${year}`;
    const curated = CURATED[key];
    add({
      id,
      kind: 'movie',
      libraryId: 1,
      title,
      year,
      tmdb: tmdbFromPath ?? MATCHED_IDS[key] ?? null,
      files: files.map(([path, seed, edition, part, resolution]) => ({
        path: `${LIBRARY_ROOT}/${path}`,
        profile: curated?.profile ?? pickProfile(seed, path),
        edition,
        part,
        resolution,
        available: true,
      })),
      addedAt: curated?.addedDaysAgo !== undefined ? now - curated.addedDaysAgo * DAY : addedAgo(key),
      runtimeMs: (curated?.runtimeMin ?? 86 + (hash32(key) % 88)) * 60_000,
      posterSource: curated ? 'provider' : posterSourceFor(key),
      progress: null,
      showId: null,
      season: null,
      episodes: [],
      children: [],
    });
  });

  // 2. Shows → seasons → episodes.
  fx.shows.forEach(([name, year, samplePath, seed, seasons], si) => {
    const showId = 1_000_000 + si;
    const key = `${name}|${year}`;
    const curated = CURATED[key];
    const epMin = curated?.runtimeMin ?? [22, 24, 30, 44, 52, 58][hash32(key) % 6];
    const profile = curated?.profile ?? pickProfile(seed, key);
    const folder = samplePath.split('/').slice(0, 2).join('/');
    const showAdded = curated?.addedDaysAgo !== undefined ? now - curated.addedDaysAgo * DAY : addedAgo(key);
    const seasonIds: Id[] = [];
    for (const [sn, eps] of Object.entries(seasons).sort((a, b) => Number(a[0]) - Number(b[0]))) {
      const season = Number(sn);
      const seasonId = 2_000_000 + si * 100 + season;
      const epIds: Id[] = [];
      for (const e of eps) {
        const epId = 3_000_000 + si * 10_000 + season * 200 + e;
        const ss = String(season).padStart(2, '0');
        const ee = String(e).padStart(2, '0');
        add({
          id: epId,
          kind: 'episode',
          libraryId: 2,
          title: `Episode ${e}`,
          year,
          tmdb: null,
          files: [{ path: `${LIBRARY_ROOT}/${folder}/Season ${ss}/${name} - S${ss}E${ee}.mkv`, profile, edition: null, part: null, resolution: null, available: true }],
          addedAt: showAdded - (eps.length - e) * 3600_000,
          runtimeMs: (epMin + (hash32(`${key}${season}${e}`) % 5) - 2) * 60_000,
          posterSource: 'frame',
          progress: null,
          showId,
          season,
          episodes: [e],
          children: [],
        });
        epIds.push(epId);
      }
      add({
        id: seasonId,
        kind: 'season',
        libraryId: 2,
        title: `Season ${season}`,
        year,
        tmdb: null,
        files: [],
        addedAt: showAdded,
        runtimeMs: 0,
        posterSource: 'provider',
        progress: null,
        showId,
        season,
        episodes: [],
        children: epIds,
      });
      seasonIds.push(seasonId);
    }
    add({
      id: showId,
      kind: 'show',
      libraryId: 2,
      title: name,
      year,
      tmdb: MATCHED_IDS[key] ?? null,
      files: [],
      addedAt: showAdded,
      runtimeMs: epMin * 60_000,
      posterSource: curated ? 'provider' : posterSourceFor(key),
      progress: null,
      showId: null,
      season: null,
      episodes: [],
      children: seasonIds,
    });
  });

  // 3. Film Archive: 10,000 generated titles across two roots, one asleep.
  for (let i = 0; i < 10_000; i++) {
    const r = rng(hash32(`archive:${i}`));
    const title = `${r() < 0.35 ? 'The ' : ''}${ARCH_A[Math.floor(r() * ARCH_A.length)]} ${ARCH_B[Math.floor(r() * ARCH_B.length)]}${ARCH_C[Math.floor(r() * ARCH_C.length)]}`;
    const year = 1928 + Math.floor(r() * 98);
    const onNas = i % 3 === 0;
    const root = ARCHIVE_ROOTS[onNas ? 1 : 0];
    const ext = ['mkv', 'mp4', 'avi', 'ts'][Math.floor(r() * 4)];
    add({
      id: 5_000_000 + i,
      kind: 'movie',
      libraryId: 3,
      title,
      year,
      tmdb: null,
      files: [{ path: `${root}/${title} (${year})/${title} (${year}).${ext}`, profile: profileKeys[Math.floor(r() * profileKeys.length)], edition: null, part: null, resolution: null, available: !onNas }],
      addedAt: addedAgo(`archive${i}`) - 200 * DAY,
      runtimeMs: (80 + Math.floor(r() * 80)) * 60_000,
      posterSource: posterSourceFor(`archive${i}`),
      progress: null,
      showId: null,
      season: null,
      episodes: [],
      children: [],
    });
  }

  const scanIdle = (seen: number, finishedAgo: number): ScanStatus => ({
    state: 'watching',
    files_seen: seen,
    files_total: seen,
    files_probed: 0,
    items_added: 0,
    items_updated: 0,
    items_removed: 0,
    started_at: now - finishedAgo - 4200,
    finished_at: now - finishedAgo,
    last_error: null,
    dirs_unavailable: 0,
  });
  /** A root as flux-catalog reports it; the asleep NAS was last seen three days ago. */
  const rootStatus = (path: string, scannedAgo: number): RootStatus => {
    const available = rootAvailable.get(path) !== false;
    return { path, available, last_seen: available ? now - scannedAgo : now - 3 * DAY };
  };
  const libraries: Library[] = [
    { id: 1, name: 'Movies', kind: 'movies', roots: [rootStatus(`${LIBRARY_ROOT}/Movies`, 42 * 60_000)], item_count: byLibrary.get(1)?.length ?? 0, scan: scanIdle(5_722, 42 * 60_000) },
    { id: 2, name: 'TV Shows', kind: 'shows', roots: [rootStatus(`${LIBRARY_ROOT}/TV`, 42 * 60_000)], item_count: byLibrary.get(2)?.length ?? 0, scan: scanIdle(10_949, 42 * 60_000) },
    {
      id: 3,
      name: 'Film Archive',
      kind: 'movies',
      roots: ARCHIVE_ROOTS.map((r) => rootStatus(r, 3 * 3600_000)),
      item_count: byLibrary.get(3)?.length ?? 0,
      scan: {
        ...scanIdle(6_667, 3 * 3600_000),
        state: 'unavailable',
        last_error: `${ARCHIVE_ROOTS[1]} is not responding (NAS asleep?). Its 3,334 items are kept and will play again when it wakes.`,
      },
    },
  ];

  // Continue watching: recognisable titles with resume points.
  const resume: [string, number, number?, number?][] = [
    ['Dune: Part Two|2024', 0.43],
    ['Severance|2022', 0.31, 2, 4],
    ['Arrival|2016', 0.72],
    ['Spirited Away|2001', 0.18],
    ['The Bear|2022', 0.55, 3, 2],
    ['Past Lives|2023', 0.64],
    ['Andor|2022', 0.22, 1, 7],
    ['Blade Runner 2049|2017', 0.81],
  ];
  const keyIndex = new Map<string, Id>();
  for (const rec of items.values()) if (rec.kind === 'movie' || rec.kind === 'show') keyIndex.set(`${rec.title}|${rec.year}`, rec.id);
  resume.forEach(([key, frac, season, ep], k) => {
    let id = keyIndex.get(key);
    if (id === undefined) return;
    if (season !== undefined && ep !== undefined) {
      const show = items.get(id);
      const sid = show?.children.find((c) => items.get(c)?.season === season);
      id = sid === undefined ? undefined : items.get(sid)?.children.find((c) => items.get(c)?.episodes[0] === ep);
      if (id === undefined) return;
    }
    const rec = items.get(id);
    if (!rec) return;
    rec.progress = { position_ms: Math.round(rec.runtimeMs * frac), duration_ms: rec.runtimeMs, watched: false, updated_at: now - (k + 1) * 5 * 3600_000 };
  });
  // Some finished titles.
  for (const rec of items.values()) {
    if (rec.kind === 'movie' && rec.libraryId === 1 && !rec.progress && hash32(`w${rec.id}`) % 9 === 0) {
      // As flux-catalog: a finished viewing clears the resume point.
      rec.progress = { position_ms: 0, duration_ms: rec.runtimeMs, watched: true, updated_at: now - (hash32(`w${rec.id}`) % 300) * DAY };
    }
  }

  // ───────────── views ─────────────

  /** `ArtRef.attribution` / `generator` as flux-meta fills them (stand-in model name). */
  const credits = (source: ArtSource): Pick<ArtRef, 'attribution' | 'generator'> => ({
    attribution: source === 'provider' ? 'TMDB' : null,
    generator: source === 'generated' ? 'mock/stand-in-image-model' : null,
  });

  const fileAvailable = (f: FileRec): boolean => f.available && [...rootAvailable].every(([root, up]) => up || !f.path.startsWith(`${root}/`));

  /** The episodes under a show or season. */
  const episodesOf = (rec: ItemRec): ItemRec[] =>
    rec.children.flatMap((c) => {
      const k = items.get(c);
      if (!k) return [];
      return k.kind === 'episode' ? [k] : episodesOf(k);
    });

  /** Leaves that carry files: the item itself, or a show's / season's episodes. */
  const leavesOf = (rec: ItemRec): ItemRec[] => (rec.kind === 'show' || rec.kind === 'season' ? episodesOf(rec) : [rec]);

  const artRef = (rec: ItemRec, kind: 'poster' | 'backdrop' | 'still'): ArtRef | null => {
    const owner = rec.showId !== null && kind !== 'still' ? items.get(rec.showId) ?? rec : rec;
    const seedKey = `${owner.title}|${owner.year}`;
    if (kind === 'still') {
      const seed = `${seedKey}|S${rec.season}E${rec.episodes[0]}`;
      return { url: `/api/v1/art/s-${rec.id}`, width: 1280, height: 720, blurhash: artBlurhash(seed, 'wide', 'provider'), source: 'frame', attribution: null, generator: null };
    }
    if (kind === 'backdrop') {
      const source: ArtSource = owner.posterSource === 'generated' ? 'generated' : owner.posterSource === 'frame' || owner.posterSource === null ? 'frame' : 'provider';
      return { url: `/api/v1/art/b-${owner.id}`, width: 1280, height: 720, blurhash: artBlurhash(seedKey, 'wide', source), source, ...credits(source) };
    }
    const src = owner.posterSource;
    if (!src) return null;
    const seed = rec.kind === 'season' ? `${seedKey}|season${rec.season}` : seedKey;
    return { url: `/api/v1/art/p-${rec.kind === 'season' ? rec.id : owner.id}`, width: 400, height: 600, blurhash: artBlurhash(seed, 'poster', src), source: src, ...credits(src) };
  };

  const probeFor = (f: FileRec, runtimeMs: number): ProbeResult => {
    const base = fx.profiles[f.profile].probe;
    const res = resolutionOf(f.resolution, base);
    return {
      ...base,
      duration_ms: runtimeMs,
      tracks: base.tracks.map((t) => (t.kind === 'video' && res ? { ...t, width: res[0], height: res[1] } : t)),
    };
  };

  const assetsFor = (rec: ItemRec): AssetInfo[] =>
    rec.files.map((f, k) => {
      const probe = probeFor(f, rec.runtimeMs);
      const v = probe.tracks.find((t) => t.kind === 'video');
      const a = probe.tracks.find((t) => t.kind === 'audio' && t.default) ?? probe.tracks.find((t) => t.kind === 'audio');
      const w = v?.width ?? 1920;
      const hdr = v?.hdr?.format ?? null;
      const mbps = w >= 3800 ? 48 : w >= 1900 ? 11 : w >= 1270 ? 5 : 2.2;
      const parts = [resName(w), v ? VIDEO_SHORT[v.codec] ?? v.codec : '', hdr && HDR_SHORT[hdr] ? HDR_SHORT[hdr] : ''].filter(Boolean);
      const audio = a ? `${AUDIO_SHORT[a.codec] ?? a.codec} ${layout(a.channels)}`.trim() : '';
      return {
        id: rec.id * 10 + k,
        path: f.path,
        size: Math.round(((mbps * 1e6) / 8) * (rec.runtimeMs / 1000)),
        container: probe.container,
        duration_ms: rec.runtimeMs,
        edition: f.edition ?? (f.part ? `Part ${f.part}` : null),
        label: [parts.join(' '), audio].filter(Boolean).join(' · '),
        hdr,
        available: fileAvailable(f),
        probe: { ...probe, bitrate: Math.round(mbps * 1e6) },
        subtitle_files: [],
      };
    });

  const subtitleFor = (rec: ItemRec): string | null => {
    if (rec.kind === 'episode') {
      const ss = String(rec.season).padStart(2, '0');
      const eps = rec.episodes.map((e) => String(e).padStart(2, '0'));
      return `S${ss}E${eps.join('–')}`;
    }
    if (rec.kind === 'season') return rec.title;
    return null;
  };

  const summaries = new Map<Id, ItemSummary>();
  const summary = (id: Id): ItemSummary | undefined => {
    const cached = summaries.get(id);
    if (cached) return cached;
    const rec = items.get(id);
    if (!rec) return undefined;
    let badges: string[] = [];
    if (rec.files.length > 0) {
      const assets = assetsFor(rec);
      badges = badgesFor(assets.find((a) => a.available)?.probe ?? assets[0]?.probe ?? null);
    } else if (rec.kind === 'show' || rec.kind === 'season') {
      let first: ItemRec | undefined = rec;
      while (first && first.files.length === 0 && first.children.length > 0) first = items.get(first.children[0]);
      if (first?.files.length) badges = badgesFor(probeFor(first.files[0], first.runtimeMs));
    }
    const show = rec.showId !== null ? items.get(rec.showId) : undefined;
    const s: ItemSummary = {
      id: rec.id,
      kind: rec.kind,
      title: rec.title,
      parent_title: rec.kind === 'episode' || rec.kind === 'season' ? (show?.title ?? null) : null,
      year: rec.year,
      subtitle: subtitleFor(rec),
      poster: artRef(rec, rec.kind === 'episode' ? 'still' : 'poster'),
      backdrop: rec.kind === 'season' ? null : artRef(rec, 'backdrop'),
      progress: rec.progress,
      badges,
      child_count: rec.kind === 'show' || rec.kind === 'season' ? rec.children.length : null,
      unwatched_count: rec.kind === 'show' || rec.kind === 'season' ? episodesOf(rec).filter((e) => !e.progress?.watched).length : null,
      available: leavesOf(rec).some((l) => l.files.some(fileAvailable)),
      added_at: rec.addedAt,
    };
    summaries.set(id, s);
    return s;
  };
  const invalidate = (id: Id) => summaries.delete(id);

  const matchFor = (rec: ItemRec): { confidence: number | null; evidence: string[]; ids: [string, string][] } => {
    if (rec.kind === 'season') return { confidence: null, evidence: [], ids: [] };
    if (rec.kind === 'episode') {
      const show = rec.showId !== null ? items.get(rec.showId) : undefined;
      const ss = String(rec.season).padStart(2, '0');
      return {
        confidence: 0.99,
        evidence: [`fast path: “${show?.title ?? ''}/Season ${ss}/” folder`, `S${ss}E${String(rec.episodes[0]).padStart(2, '0')} from the filename`, 'episode exists in the show’s season listing'],
        ids: [],
      };
    }
    const rel = rec.files[0]?.path.replace(`${LIBRARY_ROOT}/`, '') ?? '';
    const ids: [string, string][] = rec.tmdb ? [['tmdb', String(rec.tmdb)]] : [];
    if (rec.kind === 'show') {
      return {
        confidence: rec.tmdb ? 0.98 : 0.86,
        evidence: ['show folder name in a Shows library', rec.year ? `year ${rec.year} from the folder` : 'no year: matched by title, newest first', `${rec.children.length} season folders found`],
        ids,
      };
    }
    if (/tmdbid-\d+|tmdb-\d+/.test(rel)) {
      return { confidence: 1, evidence: [`provider id in the path: ${/tmdbid?-\d+/.exec(rel)?.[0] ?? ''}`, 'no search needed'], ids };
    }
    const edition = rec.files.find((f) => f.edition)?.edition;
    const editionLine = edition ? [`edition “${edition}” from the {edition-…} tag`] : [];
    if (/\/[^/]+ \(\d{4}\)\//.test(rel)) {
      return { confidence: rec.tmdb ? 0.98 : 0.94, evidence: ['fast path: “Title (Year)/” folder', ...editionLine, rec.tmdb ? 'TMDB: one exact title + year match' : 'TMDB: title + year match', 'runtime within 3 min of TMDB'], ids };
    }
    if (/\.\d{4}\.(\d{3,4}p)?/.test(rel)) {
      const tags = rel.split('/').pop()?.replace(/\.[a-z0-9]+$/, '').split(/[.-]/).filter((t) => /^(\d{3,4}p|BluRay|WEB|DL|WEBRip|HDTV|BDRip|REMUX|DVDRip|x264|x265|HEVC|AV1|XviD|H|264|AAC2|AC3|TrueHD|Atmos|7|1|0|DDP5|DTS|HD|MA|5)$/i.test(t)) ?? [];
      return { confidence: 0.9, evidence: ['hunch: scene release name', `ignored release tags: ${[...new Set(tags)].slice(0, 5).join(', ')}`, 'TMDB: title + year → one candidate'], ids };
    }
    const r = hash32(`m:${rec.id}`) % 10;
    if (r < 3) {
      return {
        confidence: 0.47,
        evidence: ['loose file in the library root', 'name is ambiguous: three TMDB candidates', 'AI hypothesis (on-device model, sees only the filename): a film titled “' + rec.title + '”', 'kept the candidate whose runtime matches (±2 min)'],
        ids,
      };
    }
    return { confidence: 0.81, evidence: ['loose file in the library root', `title and year from the filename${rec.year ? ` (${rec.year})` : ''}`, 'TMDB: best of two candidates by year'], ids };
  };

  const detail = (id: Id): ItemDetail | undefined => {
    const s = summary(id);
    const rec = items.get(id);
    if (!s || !rec) return undefined;
    const key = `${rec.title}|${rec.year}`;
    const show = rec.showId !== null ? items.get(rec.showId) : undefined;
    const curated = CURATED[show ? `${show.title}|${show.year}` : key];
    const r = rng(hash32(`g:${show?.id ?? rec.id}`));
    const genres = curated?.genres ?? [GENRES[Math.floor(r() * GENRES.length)], GENRES[Math.floor(r() * GENRES.length)]].filter((g, i, a) => a.indexOf(g) === i);
    let overview: string | null = curated?.logline ?? null;
    if (!overview && rec.kind !== 'season' && rec.kind !== 'episode') {
      overview = `A ${ADJ[Math.floor(r() * ADJ.length)]} ${genres[0].toLowerCase()} set in a ${NOUN[Math.floor(r() * NOUN.length)]}. (Placeholder text from the mock server; real overviews come from the metadata provider.)`;
    }
    if (rec.kind === 'episode' && show) overview = null;
    const m = matchFor(rec);
    return {
      ...s,
      overview,
      genres,
      runtime_ms: rec.runtimeMs || null,
      assets: assetsFor(rec),
      show_id: rec.showId,
      season_number: rec.season,
      episode_numbers: rec.episodes,
      external_ids: m.ids,
      match_confidence: m.confidence,
      match_evidence: m.evidence,
    };
  };

  const sortedCache = new Map<string, Id[]>();
  const libraryIds = (libraryId: Id, sort: LibrarySort): Id[] => {
    const cacheKey = `${libraryId}|${sort}`;
    const hit = sortedCache.get(cacheKey);
    if (hit) return hit;
    const ids = [...(byLibrary.get(libraryId) ?? [])];
    const desc = sort.startsWith('-');
    const field = desc ? sort.slice(1) : sort;
    const keyOf = (id: Id): string | number => {
      const rec = items.get(id);
      if (!rec) return '';
      if (field === 'year') return rec.year ?? 0;
      if (field === 'added') return rec.addedAt;
      return titleSortKey(rec.title);
    };
    const keyed = ids.map((id) => [keyOf(id), id] as const);
    keyed.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : a[1] - b[1]));
    if (desc) keyed.reverse();
    const out = keyed.map(([, id]) => id);
    sortedCache.set(cacheKey, out);
    return out;
  };

  const UHD = new Set(['2160p']);
  const matchesFilter = (id: Id, filter: ItemFilter): boolean => {
    const rec = items.get(id);
    if (!rec) return false;
    const leaves = leavesOf(rec);
    switch (filter) {
      case 'unwatched':
        return leaves.some((l) => !l.progress?.watched);
      case 'in_progress':
        return leaves.some((l) => l.progress !== null && !l.progress.watched && l.progress.position_ms > 0);
      case '4k':
        return leaves.some((l) => l.files.some((f) => f.resolution !== null && UHD.has(f.resolution)) || summary(l.id)?.badges.includes('4K'));
      case 'hdr':
        return leaves.some((l) => summary(l.id)?.badges.some((b) => ['HDR10', 'HDR10+', 'HLG', 'DV', 'Dolby Vision'].includes(b)));
    }
  };

  const searchIndex: [string, Id][] = [];
  for (const rec of items.values()) if (rec.kind === 'movie' || rec.kind === 'show') searchIndex.push([foldForSearch(rec.title), rec.id]);

  const search = (q: string): ItemSummary[] => {
    const needle = foldForSearch(q.trim());
    if (!needle) return [];
    const scored: [number, Id][] = [];
    for (const [hay, id] of searchIndex) {
      const at = hay.indexOf(needle);
      if (at < 0) continue;
      const wordStart = at === 0 || hay[at - 1] === ' ';
      const lib = items.get(id)?.libraryId ?? 9;
      scored.push([(at === 0 ? 0 : wordStart ? 1 : 2) * 10 + lib, id]);
    }
    scored.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
    return scored.slice(0, 40).flatMap(([, id]) => summary(id) ?? []);
  };

  const home = (): Home => {
    // Continue watching includes a rewatch in progress (as flux-catalog).
    const progressed = [...items.values()].filter((r) => r.progress && r.progress.position_ms > 0);
    progressed.sort((a, b) => (b.progress?.updated_at ?? 0) - (a.progress?.updated_at ?? 0));
    const recent = (libraryId: Id | null, n: number) => {
      const pool = libraryId === null ? [1, 2].flatMap((l) => byLibrary.get(l) ?? []) : byLibrary.get(libraryId) ?? [];
      return [...pool]
        .sort((a, b) => (items.get(b)?.addedAt ?? 0) - (items.get(a)?.addedAt ?? 0))
        .slice(0, n)
        .flatMap((id) => summary(id) ?? []);
    };
    return {
      continue_watching: progressed.slice(0, 12).flatMap((r) => summary(r.id) ?? []),
      recently_added: recent(null, 24),
      rows: libraries.map((l) => ({ title: l.name, library_id: l.id, items: recent(l.id, 24) })),
    };
  };

  const setProgress = (id: Id, positionMs: number, durationMs: number) => {
    const rec = items.get(id);
    if (!rec) return;
    // As flux-catalog: ≥ 90 % finishes (watched, resume point cleared); a rewatch keeps watched and tracks its position.
    const finished = positionMs >= durationMs * 0.9;
    rec.progress = { position_ms: finished ? 0 : positionMs, duration_ms: durationMs, watched: finished || (rec.progress?.watched ?? false), updated_at: Date.now() };
    invalidate(id);
    invalidateParents(rec);
  };

  /** A show's and its seasons' summaries count unwatched episodes. */
  const invalidateParents = (rec: ItemRec) => {
    if (rec.showId === null) return;
    invalidate(rec.showId);
    for (const sid of items.get(rec.showId)?.children ?? []) invalidate(sid);
  };

  const setWatched = (id: Id, watched: boolean) => {
    const rec = items.get(id);
    if (!rec) return;
    for (const leaf of leavesOf(rec)) {
      leaf.progress = watched ? { position_ms: 0, duration_ms: leaf.runtimeMs, watched: true, updated_at: Date.now() } : null;
      invalidate(leaf.id);
    }
    // Containers summarise their episodes (unwatched_count).
    invalidate(id);
    invalidateParents(rec);
    for (const c of rec.children) invalidate(c);
  };

  let nextNewId = 6_000_000;
  const addItems = (libraryId: Id, count: number): Id[] => {
    const lib = libraries.find((l) => l.id === libraryId);
    const added: Id[] = [];
    for (let k = 0; k < count; k++) {
      const src = items.get(10_000 + (hash32(`new${nextNewId}`) % fx.movies.length));
      if (!src) continue;
      const id = nextNewId++;
      add({ ...src, id, libraryId, addedAt: Date.now(), progress: null, files: src.files.map((f) => ({ ...f, path: `${lib?.roots[0]?.path ?? '/new'}/${f.path.split('/').slice(-2).join('/')}` })) });
      added.push(id);
    }
    for (const key of [...sortedCache.keys()]) if (key.startsWith(`${libraryId}|`)) sortedCache.delete(key);
    if (lib) lib.item_count = byLibrary.get(libraryId)?.length ?? 0;
    for (const id of added) {
      const rec = items.get(id);
      if (rec) searchIndex.push([foldForSearch(rec.title), id]);
    }
    return added;
  };

  const createLibrary = (name: string, kind: Library['kind'], roots: string[]): Library => {
    const lib: Library = {
      id: Math.max(...libraries.map((l) => l.id)) + 1,
      name,
      kind,
      roots: roots.map((path) => ({ path, available: true, last_seen: null })),
      item_count: 0,
      scan: { state: 'idle', files_seen: 0, files_total: null, files_probed: 0, items_added: 0, items_updated: 0, items_removed: 0, started_at: null, finished_at: null, last_error: null, dirs_unavailable: 0 },
    };
    for (const r of roots) rootAvailable.set(r, true);
    libraries.push(lib);
    byLibrary.set(lib.id, []);
    return lib;
  };

  return {
    libraries,
    rootAvailable,
    get: (id) => items.get(id),
    summary,
    detail,
    children: (id) => (items.get(id)?.children ?? []).flatMap((c) => summary(c) ?? []),
    libraryIds,
    matchesFilter,
    home,
    search,
    setProgress,
    setWatched,
    addItems,
    createLibrary,
    profileFile: (id, index) => {
      const p = items.get(id)?.files[index]?.profile;
      const prof = p ? fx.profiles[p] : undefined;
      return prof ? { file: prof.file, durationMs: prof.probe.duration_ms } : undefined;
    },
    now,
  };
}
