#!/usr/bin/env node
// Builds src/mock/fixtures.json from the synthetic test library's ground truth
// (library.manifest.jsonl) and the feature clips' ffprobe facts (manifest.json).
//
//   node scripts/gen-fixtures.mjs [--library PATH] [--features PATH]
//
// The defaults are $FLUX_TEST_MEDIA/library.manifest.jsonl and
// $FLUX_TEST_MEDIA/features/manifest.json (see CONTRIBUTING.md).
//
// The output is compact (arrays, not objects) because it ships in the mock
// chunk; src/mock/db.ts expands it. Only the mock backend uses it.
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { basename, dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';
import { testMedia } from './work.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const arg = (name, dflt) => {
  const i = process.argv.indexOf(name);
  return i > 0 ? process.argv[i + 1] : dflt();
};
const LIBRARY = arg('--library', () => join(testMedia(), 'library.manifest.jsonl'));
const FEATURES = arg('--features', () => join(testMedia(), 'features', 'manifest.json'));
// Recorded in the output without machine-specific prefixes.
const portable = (p) => {
  const root = process.env.FLUX_TEST_MEDIA ?? (process.env.FLUX_WORK && join(process.env.FLUX_WORK, 'media'));
  const rel = root ? relative(root, p) : '';
  return root && !rel.startsWith('..') ? `$FLUX_TEST_MEDIA/${rel}` : basename(p);
};
const OUT = join(here, '..', 'src', 'mock', 'fixtures.json');

// ffprobe codec_name → flux-core Codec (serde snake_case).
const CODEC = {
  h264: 'h264', hevc: 'hevc', av1: 'av1', vp9: 'vp9', mpeg2video: 'mpeg2_video', mpeg4: 'mpeg4_part2',
  aac: 'aac', ac3: 'ac3', eac3: 'eac3', truehd: 'true_hd', dts: 'dts', flac: 'flac', opus: 'opus', mp3: 'mp3',
  subrip: 'subrip', ass: 'ass',
};
const CODEC_TAG = {
  h264: 'V_MPEG4/ISO/AVC', hevc: 'V_MPEGH/ISO/HEVC', av1: 'V_AV1', mpeg2_video: 'V_MPEG2', mpeg4_part2: 'XVID',
  aac: 'A_AAC', ac3: 'A_AC3', eac3: 'A_EAC3', true_hd: 'A_TRUEHD', dts: 'A_DTS', flac: 'A_FLAC', opus: 'A_OPUS',
  mp3: 'A_MPEG/L3', subrip: 'S_TEXT/UTF8', ass: 'S_TEXT/ASS',
};
function containerOf(file, formatName) {
  if (file.endsWith('.webm')) return 'web_m';
  if (file.endsWith('.m2ts')) return 'm2ts';
  if (formatName.startsWith('matroska')) return 'matroska';
  if (formatName.startsWith('mov')) return 'mp4';
  if (formatName === 'mpegts') return 'mpeg_ts';
  if (formatName === 'avi') return 'avi';
  return 'unknown';
}

// The clips are synthetic (mono test tones, untagged). Give the mock the track
// metadata real releases carry so pickers and badges look like a real library.
const ENRICH = {
  'h264_multi_audio_subs.mkv': {
    1: { language: 'eng', title: 'English', channels: 2, default: true },
    2: { language: 'jpn', title: 'Japanese 5.1' },
    3: { language: 'eng', title: 'English', default: true },
    4: { language: 'eng', title: 'English (Signs & Songs)' },
  },
  'h264_truehd71.mkv': { 1: { language: 'eng', title: 'TrueHD Atmos 7.1', channels: 8, default: true } },
  'h264_dts51.mkv': { 1: { language: 'eng', title: 'DTS-HD MA 5.1', profile: 'DTS-HD MA', default: true } },
  'h264_flac.mkv': { 1: { language: 'eng', title: 'FLAC Stereo', channels: 2, default: true } },
  'hevc_main10_hdr10_eac3.mkv': { 1: { language: 'eng', title: 'Dolby Digital Plus 5.1', default: true } },
  'hevc_4k_60mbps.mkv': { 1: { language: 'eng', default: true } },
  'mpeg2_ac3.ts': { 1: { language: 'eng', default: true } },
  'av1_opus.mkv': { 1: { language: 'eng', channels: 2, default: true } },
};
const HDR10 = {
  format: 'hdr10',
  mastering: { primaries: [[13250, 34500], [7500, 3000], [34000, 16000]], white_point: [15635, 16450], max_luminance: 10000000, min_luminance: 50 },
  cll: { max_cll: 1000, max_fall: 400 },
  dovi: null,
};

function toProfile(entry) {
  const { file, bytes } = entry;
  const fmt = entry.ffprobe.format;
  const duration = Math.round(parseFloat(fmt.duration) * 1000);
  const enrich = ENRICH[file] ?? {};
  const tracks = entry.ffprobe.streams.map((s) => {
    const codec = CODEC[s.codec_name] ?? 'other';
    const kind = s.codec_type === 'video' ? 'video' : s.codec_type === 'audio' ? 'audio' : 'subtitle';
    const tenBit = (s.pix_fmt ?? '').includes('10');
    const isHdr = kind === 'video' && file.includes('hdr10');
    const e = enrich[s.index] ?? {};
    return {
      index: s.index,
      id: s.index + 1,
      kind,
      codec,
      codec_tag: CODEC_TAG[codec] ?? s.codec_name,
      profile: e.profile ?? s.profile ?? null,
      level: kind === 'video' ? (codec === 'hevc' ? 153 : codec === 'h264' ? 41 : null) : null,
      bit_depth: kind === 'video' ? (tenBit ? 10 : 8) : null,
      width: s.width ?? null,
      height: s.height ?? null,
      // Every feature clip has square pixels (ffprobe: 1:1).
      sample_aspect_ratio: kind === 'video' ? { num: 1, den: 1 } : null,
      frame_rate: kind === 'video' ? { num: 24000, den: 1001 } : null,
      interlaced: kind === 'video' ? false : null,
      color: kind === 'video'
        ? isHdr
          ? { primaries: 9, transfer: 16, matrix: 9, full_range: false }
          : { primaries: 1, transfer: 1, matrix: 1, full_range: false }
        : null,
      hdr: kind === 'video' ? (isHdr ? HDR10 : { format: 'sdr', mastering: null, cll: null, dovi: null }) : null,
      channels: kind === 'audio' ? (e.channels ?? s.channels ?? 2) : null,
      sample_rate: kind === 'audio' ? 48000 : null,
      language: e.language ?? (s.tags?.language && s.tags.language !== 'und' ? s.tags.language : null),
      title: e.title ?? null,
      default: e.default ?? s.index <= 1,
      forced: false,
      hearing_impaired: false,
      bitrate: null,
    };
  });
  return {
    file,
    bytes,
    probe: {
      container: containerOf(file, fmt.format_name),
      duration_ms: duration,
      bitrate: Number(fmt.bit_rate) || null,
      title: null,
      tracks,
      chapters: file === 'h264_multi_audio_subs.mkv'
        ? [{ start_ms: 0, end_ms: 20000, title: 'Opening' }, { start_ms: 20000, end_ms: 40000, title: 'Middle' }, { start_ms: 40000, end_ms: null, title: 'End' }]
        : [],
      attachments: file.endsWith('.mkv') && tracks.some((t) => t.codec === 'ass')
        ? [{ name: 'OpenSans-Semibold.ttf', mime: 'font/ttf', size: 221328 }]
        : [],
      has_index: !file.endsWith('.ts') && !file.endsWith('.m2ts') && !file.endsWith('.avi'),
      faststart: file.endsWith('.mp4') ? !file.includes('moov_end') : null,
      needs_frame_sei: file === 'hevc_main10_hdr10_eac3.mkv',
      warnings: [],
      probe_version: 1,
      bytes_read: file.endsWith('.mp4') && file.includes('moov_end') ? 196608 : 131072,
      reads: file.includes('moov_end') ? 3 : 2,
    },
  };
}

// Seed clip used by the library generator → feature clips with the same codecs.
const SEED_PROFILES = {
  h264_mkv: ['h264_multi_audio_subs.mkv', 'h264_truehd71.mkv', 'h264_dts51.mkv', 'h264_flac.mkv', 'h264_long_gop.mkv'],
  hevc_mkv: ['hevc_main10_hdr10_eac3.mkv', 'hevc_4k_60mbps.mkv'],
  h264_mp4: ['h264_aac_faststart.mp4'],
  h264_mp4_tail: ['h264_aac_moov_end.mp4'],
  av1_mkv: ['av1_opus.mkv'],
  mpeg4_avi: ['mpeg4_mp3.avi'],
  h264_ts: ['mpeg2_ac3.ts', 'h264_aac.m2ts'],
};

const features = JSON.parse(readFileSync(FEATURES, 'utf8'));
const profiles = {};
for (const entry of features) profiles[entry.file] = toProfile(entry);

const rows = readFileSync(LIBRARY, 'utf8').trim().split('\n').map((l) => JSON.parse(l));

// Movies: group files by (title, year); each file is a version / edition / part.
const movies = new Map();
for (const r of rows) {
  if (r.kind !== 'movie') continue;
  const key = `${r.title}\u0000${r.year ?? ''}`;
  let m = movies.get(key);
  if (!m) {
    m = { title: r.title, year: r.year ?? null, files: [], tmdb: null };
    movies.set(key, m);
  }
  if (r.tmdb_id) m.tmdb = r.tmdb_id;
  m.files.push([r.path, r.seed ?? 'h264_mkv', r.edition ?? null, r.part ?? null, r.resolution ?? null]);
}

// Shows: (show, year) → season → episode numbers; keep one sample path for the naming style.
const shows = new Map();
for (const r of rows) {
  if (r.kind !== 'episode') continue;
  const key = `${r.show}\u0000${r.year ?? ''}`;
  let s = shows.get(key);
  if (!s) {
    s = { name: r.show, year: r.year ?? null, seasons: {}, sample: r.path, seed: r.seed ?? 'h264_mkv', files: 0 };
    shows.set(key, s);
  }
  s.files += 1;
  const eps = (s.seasons[r.season] ??= []);
  for (const e of r.episodes) if (!eps.includes(e)) eps.push(e);
}
for (const s of shows.values()) for (const k of Object.keys(s.seasons)) s.seasons[k].sort((a, b) => a - b);

const out = {
  generated_from: { library: portable(LIBRARY), features: portable(FEATURES) },
  seed_profiles: SEED_PROFILES,
  profiles,
  // [title, year, tmdb|null, [[path, seed, edition, part, resolution], ...]]
  movies: [...movies.values()].map((m) => [m.title, m.year, m.tmdb, m.files]),
  // [name, year, samplePath, seed, {season: [episodes]}]
  shows: [...shows.values()].map((s) => [s.name, s.year, s.sample, s.seed, s.seasons]),
};
mkdirSync(dirname(OUT), { recursive: true });
writeFileSync(OUT, JSON.stringify(out));
console.log(`wrote ${OUT}: ${out.movies.length} movies, ${out.shows.length} shows, ${Object.keys(profiles).length} media profiles`);
