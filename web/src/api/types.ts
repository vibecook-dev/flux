/**
 * Hand-written mirror of flux-core's serde wire types:
 * `crates/flux-core/src/{media,names,plan,api}.rs`.
 *
 * Conventions (serde defaults used by flux-core):
 * - `Option<T>` is always serialized, as `T | null` (no `skip_serializing_if`).
 * - Unit enums are `rename_all = "snake_case"` strings. serde's rule inserts `_`
 *   before every uppercase letter, so `WebM` → `"web_m"`, `TrueHd` → `"true_hd"`,
 *   `DoviP7To81` → `"dovi_p7_to81"`.
 * - Data-carrying enums are internally tagged: `VideoOp`/`AudioOp`/`SubtitleOp`
 *   by `"op"`, `Reason` by `"reason"`, `Event` by `"type"`.
 * - `ItemDetail` flattens `ItemSummary` into itself.
 * - Tuples (`Vec<(String, String)>`) are JSON arrays.
 *
 * Every enum below is declared as a `const` array so a test can compare it with
 * the golden JSON emitted by flux-core itself (`pnpm golden`, see
 * `src/api/golden.test.ts`), and `src/api/__golden__/golden.check.ts` makes tsc
 * check full sample payloads against these interfaces.
 */

/** Database id (Rust `i64`). */
export type Id = number;

// ───────────────────────────── media.rs ─────────────────────────────

export const CONTAINERS = ['matroska', 'web_m', 'mp4', 'mov', 'mpeg_ts', 'm2ts', 'avi', 'unknown'] as const;
/** Container formats, sniffed from magic bytes. */
export type Container = (typeof CONTAINERS)[number];

export const TRACK_KINDS = ['video', 'audio', 'subtitle', 'attachment', 'data'] as const;
export type TrackKind = (typeof TRACK_KINDS)[number];

export const CODECS = [
  // video
  'h264', 'hevc', 'av1', 'vp9', 'vp8', 'mpeg2_video', 'mpeg4_part2', 'vc1', 'pro_res', 'mjpeg',
  // audio
  'aac', 'ac3', 'eac3', 'true_hd', 'dts', 'flac', 'opus', 'vorbis', 'mp3', 'mp2', 'pcm', 'alac',
  // subtitles
  'subrip', 'ass', 'ssa', 'web_vtt', 'mov_text', 'pgs', 'vob_sub', 'dvb_sub',
  'other',
] as const;
export type Codec = (typeof CODECS)[number];

export const HDR_FORMATS = ['sdr', 'hdr10', 'hdr10_plus', 'hlg', 'dolby_vision'] as const;
export type HdrFormat = (typeof HDR_FORMATS)[number];

/** Exact rational (frame rates, aspect ratios). */
export interface Rational {
  num: number;
  den: number;
}

/** ITU-T H.273 code points. */
export interface ColorInfo {
  primaries: number | null;
  transfer: number | null;
  matrix: number | null;
  full_range: boolean | null;
}

/** SMPTE ST 2086; chromaticities in 0.00002, luminance in 0.0001 cd/m². */
export interface MasteringDisplay {
  /** [G, B, R] as (x, y). */
  primaries: [[number, number], [number, number], [number, number]];
  white_point: [number, number];
  max_luminance: number;
  min_luminance: number;
}

export interface ContentLightLevel {
  max_cll: number;
  max_fall: number;
}

export interface DoviConfig {
  profile: number;
  level: number;
  rpu_present: boolean;
  el_present: boolean;
  bl_present: boolean;
  bl_signal_compat_id: number;
}

export interface HdrInfo {
  format: HdrFormat;
  mastering: MasteringDisplay | null;
  cll: ContentLightLevel | null;
  dovi: DoviConfig | null;
}

export interface Track {
  /** 0-based order of the track within the file. */
  index: number;
  id: number;
  kind: TrackKind;
  codec: Codec;
  codec_tag: string;
  profile: string | null;
  level: number | null;
  bit_depth: number | null;
  width: number | null;
  height: number | null;
  /** Pixel shape (width:height), e.g. 32:27 anamorphic; `null` = not stated (square). */
  sample_aspect_ratio: Rational | null;
  frame_rate: Rational | null;
  interlaced: boolean | null;
  color: ColorInfo | null;
  hdr: HdrInfo | null;
  channels: number | null;
  sample_rate: number | null;
  language: string | null;
  title: string | null;
  default: boolean;
  forced: boolean;
  hearing_impaired: boolean;
  bitrate: number | null;
}

export interface Chapter {
  start_ms: number;
  end_ms: number | null;
  title: string | null;
}

export interface Attachment {
  name: string;
  mime: string;
  size: number;
}

export interface ProbeResult {
  container: Container;
  duration_ms: number | null;
  bitrate: number | null;
  title: string | null;
  tracks: Track[];
  chapters: Chapter[];
  attachments: Attachment[];
  has_index: boolean;
  faststart: boolean | null;
  needs_frame_sei: boolean;
  warnings: string[];
  probe_version: number;
  bytes_read: number;
  reads: number;
}

// ───────────────────────────── names.rs ─────────────────────────────

export const LIBRARY_KINDS = ['movies', 'shows', 'mixed'] as const;
export type LibraryKind = (typeof LIBRARY_KINDS)[number];

export const GUESS_KINDS = ['movie', 'episode', 'extra', 'sample', 'unknown'] as const;
export type GuessKind = (typeof GUESS_KINDS)[number];

export const EXTRA_KINDS = [
  'trailer', 'featurette', 'behind_the_scenes', 'deleted_scene', 'interview', 'short', 'scene', 'other',
] as const;
export type ExtraKind = (typeof EXTRA_KINDS)[number];

export interface ProviderIds {
  tmdb: number | null;
  imdb: string | null;
  tvdb: number | null;
}

export interface NameGuess {
  kind: GuessKind;
  title: string | null;
  /** The other-script half of a bilingual name ("示例剧" beside "Some Show"). */
  original_title: string | null;
  year: number | null;
  season: number | null;
  episodes: number[];
  absolute_episode: number | null;
  air_date: string | null;
  episode_title: string | null;
  edition: string | null;
  part: number | null;
  resolution: string | null;
  source: string | null;
  video_codec: string | null;
  release_group: string | null;
  extra: ExtraKind | null;
  ids: ProviderIds;
  confidence: number;
  evidence: string[];
}

// ───────────────────────────── plan.rs ─────────────────────────────

export const DELIVERY_FORMATS = ['progressive_file', 'hls_fmp4', 'hls_ts'] as const;
export type DeliveryFormat = (typeof DELIVERY_FORMATS)[number];

export interface VideoCap {
  codec: Codec;
  /** Supported profile names, e.g. ["main", "main10"]; empty = any. */
  profiles: string[];
  max_level: number | null;
  max_bit_depth: number | null;
  max_width: number | null;
  max_height: number | null;
  /** HDR formats the client can display as-is (otherwise the server tone-maps). */
  hdr: HdrFormat[];
  dovi_profiles: number[];
}

export interface AudioCap {
  codec: Codec;
  max_channels: number;
}

/** What a client says it can play (see `src/caps/detect.ts`). */
export interface ClientCaps {
  /** e.g. "web/chrome-141/macos". */
  client: string;
  delivery: DeliveryFormat[];
  progressive_containers: Container[];
  video: VideoCap[];
  audio: AudioCap[];
  /**
   * The display shows HDR right now (`(dynamic-range: high)`). `VideoCap.hdr`
   * already says what the client presents correctly (an HDR display, or the
   * platform tone-maps itself); this is the display alone.
   */
  display_hdr: boolean;
  subtitles: Codec[];
  needs_demuxed_audio: boolean;
  max_bitrate: number | null;
}

export const BITSTREAM_FIXES = ['dovi_p7_to81', 'strip_inband_parameter_sets'] as const;
export type BitstreamFix = (typeof BITSTREAM_FIXES)[number];

export type VideoOp =
  | { op: 'copy'; fixes: BitstreamFix[] }
  | {
      op: 'transcode';
      codec: Codec;
      width: number;
      height: number;
      bitrate: number;
      tonemap: boolean;
      burn_subtitle: number | null;
      hardware: boolean;
    };

export type AudioOp = { op: 'copy' } | { op: 'transcode'; codec: Codec; channels: number; bitrate: number };

export type SubtitleOp = { op: 'web_vtt' } | { op: 'client_render' } | { op: 'burn_in' };

/** Why a plan is not a plain direct play (tag: `"reason"`). */
export type Reason =
  | { reason: 'container_unsupported'; container: Container }
  | { reason: 'video_codec_unsupported'; codec: Codec }
  | { reason: 'video_profile_unsupported'; codec: Codec; profile: string }
  | { reason: 'video_level_too_high'; codec: Codec; level: number; max: number }
  | { reason: 'video_bit_depth_unsupported'; bit_depth: number }
  | { reason: 'video_resolution_too_high'; width: number; height: number }
  | { reason: 'hdr_unsupported'; format: HdrFormat }
  | { reason: 'dovi_profile_unsupported'; profile: number }
  | { reason: 'audio_codec_unsupported'; codec: Codec }
  | { reason: 'audio_channels_too_many'; channels: number; max: number }
  | { reason: 'subtitle_needs_burn_in'; codec: Codec }
  | { reason: 'bitrate_too_high'; bitrate: number; max: number }
  | { reason: 'no_index' }
  | { reason: 'codec_not_packageable'; codec: Codec }
  | { reason: 'client_quirk'; note: string };

export interface AudioPlan {
  track: number;
  op: AudioOp;
}

export interface SubtitlePlan {
  track: number;
  op: SubtitleOp;
}

export interface PlaybackPlan {
  delivery: DeliveryFormat;
  video_track: number | null;
  video: VideoOp;
  audio: AudioPlan[];
  subtitles: SubtitlePlan[];
  demuxed_audio: boolean;
  reasons: Reason[];
  est_cpu_ms_per_min: number;
}

/** `PlaybackPlan::label()` values. */
export type PlanLabel = 'Direct play' | 'Direct stream' | 'Audio conversion' | 'Transcode';

// ───────────────────────────── api.rs ─────────────────────────────

export const ROLES = ['owner', 'admin', 'viewer', 'kid', 'guest'] as const;
export type Role = (typeof ROLES)[number];

/** Tailnet identity of the caller (Tailscale WhoIs). */
export interface Identity {
  login_name: string;
  display_name: string;
  profile_pic_url: string | null;
  node_name: string | null;
  role: Role;
  /** Tailscale ACL tags of the device (`tag:tv`); non-empty = no human owner. */
  tags: string[];
}

/**
 * How the client reached the server. Free-form in Rust; these are the values
 * api.rs documents plus flux-net's `NetPath::as_str()` ("tailnet" = path unknown).
 */
export type NetworkPath = 'lan' | 'tailnet-direct' | 'tailnet-relay' | 'tailnet' | 'localhost' | (string & {});

export interface ServerInfo {
  name: string;
  version: string;
  you: Identity | null;
  path: NetworkPath;
  /** MagicDNS name when the server is on a tailnet; `null` = not connected. */
  tailnet_name: string | null;
}

export interface CreateLibrary {
  name: string;
  kind: LibraryKind;
  roots: string[];
}

export const SCAN_STATES = ['idle', 'scanning', 'watching', 'unavailable', 'error'] as const;
export type ScanState = (typeof SCAN_STATES)[number];

export interface ScanStatus {
  state: ScanState;
  files_seen: number;
  /** Known once the walk is done (`null` while walking); equals `files_seen` at rest. */
  files_total: number | null;
  files_probed: number;
  items_added: number;
  items_updated: number;
  items_removed: number;
  /** Unix epoch milliseconds. */
  started_at: number | null;
  finished_at: number | null;
  last_error: string | null;
  /** Folders that didn't answer in the last scan (nothing under them was removed). */
  dirs_unavailable: number;
}

/** One folder of a library and whether it is reachable. */
export interface RootStatus {
  path: string;
  /** `false` while it doesn't answer (NAS asleep, drive unplugged); items are kept. */
  available: boolean;
  /** Unix ms when last confirmed reachable; `null` = never scanned. */
  last_seen: number | null;
}

export interface Library {
  id: Id;
  name: string;
  kind: LibraryKind;
  roots: RootStatus[];
  item_count: number;
  scan: ScanStatus;
}

export interface Page<T> {
  items: T[];
  /** Opaque cursor for the next page; `null` at the end. */
  next: string | null;
  total: number | null;
}

/** Sort orders for `GET /libraries/{id}/items?sort=` (`ItemSort`). */
export const LIBRARY_SORTS = ['title', '-title', 'year', '-year', 'added', '-added'] as const;
export type LibrarySort = (typeof LIBRARY_SORTS)[number];

/** Filters for `?filter=` (`ItemFilter`): the library chips. */
export const ITEM_FILTERS = ['unwatched', 'in_progress', '4k', 'hdr'] as const;
export type ItemFilter = (typeof ITEM_FILTERS)[number];

/** Query of `GET /api/v1/libraries/{id}/items` (every key optional on the wire). */
export interface ItemsParams {
  cursor: string | null;
  limit: number | null;
  sort: LibrarySort | null;
  filter: ItemFilter | null;
}

export const ITEM_KINDS = ['movie', 'show', 'season', 'episode', 'extra'] as const;
export type ItemKind = (typeof ITEM_KINDS)[number];

export const ART_SOURCES = ['provider', 'local', 'frame', 'generated'] as const;
/** The UI must label `frame` and `generated` art. */
export type ArtSource = (typeof ART_SOURCES)[number];

/** Image reference; append `?w=` to `url` to resize. */
export interface ArtRef {
  url: string;
  width: number | null;
  height: number | null;
  blurhash: string | null;
  source: ArtSource;
  /** Who to credit for provider art, e.g. "TMDB". */
  attribution: string | null;
  /** The model that made `generated` art. */
  generator: string | null;
}

export interface Progress {
  position_ms: number;
  duration_ms: number;
  watched: boolean;
  updated_at: number;
}

export interface ItemSummary {
  id: Id;
  kind: ItemKind;
  /** The item's own name (episode title, "Season 2", movie or show title). */
  title: string;
  /** Show name for seasons and episodes; parent work for extras. */
  parent_title: string | null;
  year: number | null;
  /** "S01E03" for episodes, season name for seasons. */
  subtitle: string | null;
  poster: ArtRef | null;
  backdrop: ArtRef | null;
  progress: Progress | null;
  /** "4K", "HDR10", "DV", "Atmos", "5.1"… */
  badges: string[];
  child_count: number | null;
  /** Shows and seasons: episodes not watched by the caller. */
  unwatched_count: number | null;
  /** `false` when no file of the item is on a reachable root (Play answers 409). */
  available: boolean;
  added_at: number;
}

/** One playable file of an item (versions, editions). */
export interface AssetInfo {
  id: Id;
  path: string;
  size: number;
  container: Container;
  duration_ms: number | null;
  edition: string | null;
  /** "4K HEVC HDR10 · TrueHD 7.1" */
  label: string;
  hdr: HdrFormat | null;
  /** `false` when the source root is offline. */
  available: boolean;
  probe: ProbeResult | null;
}

/** `ItemSummary` flattened together with the detail fields. */
export interface ItemDetail extends ItemSummary {
  overview: string | null;
  genres: string[];
  runtime_ms: number | null;
  assets: AssetInfo[];
  show_id: Id | null;
  season_number: number | null;
  episode_numbers: number[];
  /** `(provider, id)` pairs, e.g. `["tmdb", "693134"]`. */
  external_ids: [string, string][];
  match_confidence: number | null;
  match_evidence: string[];
}

export interface HomeRow {
  title: string;
  library_id: Id | null;
  items: ItemSummary[];
}

export interface Home {
  continue_watching: ItemSummary[];
  recently_added: ItemSummary[];
  rows: HomeRow[];
}

export interface PlayRequest {
  item_id: Id;
  asset_id: Id | null;
  caps: ClientCaps;
  audio_track: number | null;
  subtitle_track: number | null;
  /** `null` = resume point (or 0). */
  start_ms: number | null;
}

export interface SubtitleRef {
  track: number;
  label: string;
  language: string | null;
  url: string;
  forced: boolean;
}

export interface PlayResponse {
  session_id: string;
  asset_id: Id;
  plan: PlaybackPlan;
  /** HLS multivariant playlist or progressive file URL. */
  url: string;
  /** Human sentences from `plan.reasons`. */
  explain: string[];
  start_ms: number;
  duration_ms: number | null;
  /** Init + first segment: fetch right away to cut TTFF. */
  prefetch: string[];
  /** HLS audio renditions in playlist order (empty for direct play). */
  audio: AudioRef[];
  subtitles: SubtitleRef[];
}

/** One HLS audio rendition (`a/{rendition}/index.m3u8`). */
export interface AudioRef {
  /** "1", "1-aac2", "1-eac36". */
  rendition: string;
  track: number;
  /** The playlist NAME, e.g. "English (AAC Stereo)". */
  label: string;
  language: string | null;
  codec: Codec;
  channels: number;
  default: boolean;
}

/** `POST /api/v1/plan` (dry run of `/play`). */
export interface PlanPreview {
  asset_id: Id;
  plan: PlaybackPlan;
  explain: string[];
  label: PlanLabel;
}

/** `POST /api/v1/items/{id}/watched` → 204. */
export interface MarkWatched {
  watched: boolean;
}

export interface ProgressReport {
  position_ms: number;
  paused: boolean;
  ttff_ms: number | null;
  last_seek_ms: number | null;
  stalls: number | null;
  dropped_frames: number | null;
}

export interface SessionDiagnostics {
  session_id: string;
  plan: PlaybackPlan;
  explain: string[];
  path: NetworkPath;
  segments_served: number;
  bytes_served: number;
  segment_ms_p50: number | null;
  segment_ms_p95: number | null;
  client: ProgressReport | null;
}

/** Server-Sent Events payloads (tag: `"type"`). */
export type FluxEvent =
  | { type: 'scan_progress'; library_id: Id; status: ScanStatus }
  | { type: 'items_changed'; library_id: Id; added: Id[]; updated: Id[]; removed: Id[] }
  | { type: 'source_availability'; library_id: Id; root: string; available: boolean }
  | { type: 'tailnet'; info: TailnetInfo };

export const TAILNET_MODES = ['off', 'embedded', 'system'] as const;
/** How flux reaches the tailnet: not at all, its own node (Truffle), or this machine's tailscaled. */
export type TailnetMode = (typeof TAILNET_MODES)[number];

export const TAILNET_STATES = ['off', 'starting', 'needs_login', 'needs_approval', 'running', 'unavailable', 'error'] as const;
/** flux-net's `TailnetStatus`, flattened. */
export type TailnetState = (typeof TAILNET_STATES)[number];

/** `GET /api/v1/tailnet`, `POST /api/v1/tailnet/connect`, SSE `tailnet`. */
export interface TailnetInfo {
  mode: TailnetMode;
  state: TailnetState;
  /** Interactive login URL while `needs_login`. */
  auth_url: string | null;
  /** Admin-console page while `needs_approval`. */
  approve_url: string | null;
  /** The tailnet's DNS suffix, e.g. "tail1234.ts.net". */
  tailnet_name: string | null;
  /** This server's MagicDNS name, e.g. "flux.tail1234.ts.net". */
  dns_name: string | null;
  /** Where tailnet devices open flux. */
  url: string | null;
  /** Login that authorised the node; the flux owner. */
  owner_login: string | null;
  /** Unix ms when the node key expires; `null` = unknown or expiry disabled. */
  key_expiry: number | null;
  /** A system tailscaled is available on this machine. */
  system_tailscale: boolean;
  last_error: string | null;
}

/** `POST /api/v1/tailnet/connect` body. */
export interface TailnetConnect {
  mode: TailnetMode;
}

// ─────────────── web-only (proposed; NOT in flux-core yet) ───────────────
// Invites are what this client needs beyond api.rs. They are listed as a
// proposed core change in docs/prototype/web.md; the mock backend implements them.

export type InviteKind = 'share' | 'household';

/** `POST /api/v1/invites`. */
export interface CreateInvite {
  kind: InviteKind;
  role: Role;
  note: string | null;
}

/** A Tailscale node-share or tailnet-member invite. */
export interface Invite {
  id: string;
  kind: InviteKind;
  role: Role;
  note: string | null;
  url: string;
  created_at: number;
  expires_at: number | null;
  accepted: number;
}
