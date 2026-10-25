/**
 * The TS mirror against flux-core's real serde output (scripts/gen-golden.mjs).
 * Shapes are checked by tsc via golden.check.ts; this checks the values.
 */
import { describe, expect, it } from 'vitest';
import golden from './__golden__/golden.json';
import { CODEC_NAMES, CONTAINER_NAMES, explainReason, HDR_NAMES, planLabel } from './explain';
import type { PlaybackPlan, Reason } from './types';
import {
  ART_SOURCES,
  BITSTREAM_FIXES,
  CODECS,
  CONTAINERS,
  DELIVERY_FORMATS,
  EXTRA_KINDS,
  FOLDER_SORTS,
  GUESS_KINDS,
  HDR_FORMATS,
  ITEM_FILTERS,
  ITEM_KINDS,
  LIBRARY_KINDS,
  LIBRARY_SORTS,
  METADATA_SOURCES,
  ROLES,
  SCAN_STATES,
  TAILNET_MODES,
  TAILNET_STATES,
  TRACK_KINDS,
} from './types';
import {
  ART_KINDS,
  ART_ORIGINS,
  CHANGE_STATES,
  CHANGESET_STATES,
  EDIT_MODES,
  EFFECTS,
  ERROR_CODES,
  GAPS,
  LENS_INPUTS,
  MATCH_STATES,
  SOURCES,
} from './agent';

describe('enum strings match serde', () => {
  const cases: [string, readonly string[]][] = [
    ['Container', CONTAINERS],
    ['TrackKind', TRACK_KINDS],
    ['Codec', CODECS],
    ['HdrFormat', HDR_FORMATS],
    ['LibraryKind', LIBRARY_KINDS],
    ['GuessKind', GUESS_KINDS],
    ['ExtraKind', EXTRA_KINDS],
    ['DeliveryFormat', DELIVERY_FORMATS],
    ['BitstreamFix', BITSTREAM_FIXES],
    ['Role', ROLES],
    ['ScanState', SCAN_STATES],
    ['ItemKind', ITEM_KINDS],
    ['ArtSource', ART_SOURCES],
    ['ItemSort', LIBRARY_SORTS],
    ['FolderSort', FOLDER_SORTS],
    ['ItemFilter', ITEM_FILTERS],
    ['TailnetMode', TAILNET_MODES],
    ['TailnetState', TAILNET_STATES],
    ['MetadataSource', METADATA_SOURCES],
    ['Gap', GAPS],
    ['MatchState', MATCH_STATES],
    ['Source', SOURCES],
    ['ChangesetState', CHANGESET_STATES],
    ['ChangeState', CHANGE_STATES],
    ['ArtKind', ART_KINDS],
    ['ArtOrigin', ART_ORIGINS],
    ['EditMode', EDIT_MODES],
    ['ErrorCode', ERROR_CODES],
    ['LensInput', LENS_INPUTS],
    ['Effect', EFFECTS],
  ];
  for (const [name, values] of cases) {
    it(name, () => {
      expect([...values]).toEqual((golden.enums as Record<string, string[]>)[name]);
    });
  }

  it('covers the serde snake_case corner cases', () => {
    expect(CONTAINERS).toContain('web_m');
    expect(CODECS).toContain('true_hd');
    expect(BITSTREAM_FIXES).toContain('dovi_p7_to81');
    expect(HDR_FORMATS).toContain('hdr10_plus');
  });
});

describe('Reason::explain mirror', () => {
  for (const [reason, sentence] of golden.reasons as [Reason, string][]) {
    it(reason.reason, () => {
      expect(explainReason(reason)).toBe(sentence);
    });
  }

  it('uses display names, never Rust Debug names', () => {
    for (const [, sentence] of golden.reasons as [Reason, string][]) {
      expect(sentence).not.toMatch(/\b(TrueHd|Hevc|Matroska|DolbyVision|Eac3)\b/);
    }
  });

  it("rounds bitrates like Rust's {:.0} (ties to even)", () => {
    expect(explainReason({ reason: 'bitrate_too_high', bitrate: 2_500_000, max: 3_500_000 })).toContain('(2 Mb/s)');
    expect(explainReason({ reason: 'bitrate_too_high', bitrate: 2_500_000, max: 3_500_000 })).toContain('(4 Mb/s)');
  });
});

describe('display names match flux-core display_name()', () => {
  const cases: [string, Record<string, string>][] = [
    ['Codec', CODEC_NAMES],
    ['Container', CONTAINER_NAMES],
    ['HdrFormat', HDR_NAMES],
  ];
  for (const [name, table] of cases) {
    it(name, () => {
      expect(table).toEqual((golden.display as Record<string, Record<string, string>>)[name]);
    });
  }
});

describe('PlaybackPlan::label mirror', () => {
  for (const [plan, label] of golden.plan_labels as [PlaybackPlan, string][]) {
    it(label, () => {
      expect(planLabel(plan)).toBe(label);
    });
  }

  it('nests the AudioOp tag inside AudioPlan (serde internally tagged)', () => {
    const plan = (golden.plan_labels as [PlaybackPlan, string][])[0][0];
    expect(plan.audio[0]).toEqual({ track: 1, op: { op: 'copy' } });
  });
});

describe('flattened ItemDetail', () => {
  it('has ItemSummary fields at the top level and tuple external ids', () => {
    const d = golden.samples.ItemDetail as Record<string, unknown>;
    expect(d.title).toBe('Dune: Part Two');
    expect(d).not.toHaveProperty('summary');
    expect(d.external_ids).toEqual([['tmdb', '693134']]);
  });

  it('serializes Option::None as null, never omitted', () => {
    const d = golden.samples.ItemDetail as Record<string, unknown>;
    expect(d).toHaveProperty('show_id', null);
    expect(d).toHaveProperty('subtitle', null);
  });
});
