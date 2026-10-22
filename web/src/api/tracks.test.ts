/** Version names: every version of a title must be told apart in the picker (e2e P2 #7). */
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { createMockDb, type Fixtures } from '../mock/db';
import { languageName, subtitleChoices, subtitleFileLabel, versionNames } from './tracks';
import type { AssetInfo } from './types';

const fx = JSON.parse(readFileSync(new URL('../mock/fixtures.json', import.meta.url), 'utf8')) as Fixtures;

let nextId = 1;
const asset = (file: string, path: string, edition: string | null = null): AssetInfo => {
  const probe = fx.profiles[file].probe;
  return {
    id: nextId++,
    path,
    size: 1,
    container: probe.container,
    duration_ms: probe.duration_ms,
    edition,
    label: '',
    hdr: null,
    available: true,
    probe,
    subtitle_files: [],
  };
};

describe('version picker names', () => {
  it('two 2160p versions no longer both read “2160p” (the e2e Main10 case: E-AC-3 MKV vs AAC MP4)', () => {
    const names = versionNames([
      asset('hevc_main10_hdr10_eac3.mkv', '/m/Film (2020)/Film (2020) - 2160p.mkv'),
      asset('hevc_main10_hdr10_hvc1.mp4', '/m/Film (2020)/Film (2020) - 2160p.mp4'),
    ]);
    expect(names).toEqual(['2160p · HEVC · HDR10 · E-AC-3 · MKV', '2160p · HEVC · HDR10 · AAC · MP4']);
    expect(new Set(names).size).toBe(2);
  });

  it('keeps the short name when it is already unique', () => {
    expect(versionNames([asset('hevc_main10_hdr10_eac3.mkv', '/m/a.mkv'), asset('h264_aac_faststart.mp4', '/m/a.mp4')])).toEqual(['2160p', '1080p']);
    expect(versionNames([asset('h264_aac_faststart.mp4', '/m/a.mp4', 'Director’s Cut'), asset('h264_aac_faststart.mp4', '/m/b.mp4')])).toEqual([
      'Director’s Cut',
      '1080p',
    ]);
  });

  it('editions that collide keep the edition in front', () => {
    const names = versionNames([
      asset('hevc_main10_hdr10_eac3.mkv', '/m/x.mkv', 'Extended'),
      asset('h264_aac_faststart.mp4', '/m/y.mp4', 'Extended'),
    ]);
    expect(names).toEqual(['Extended · 2160p · HEVC · HDR10 · E-AC-3 · MKV', 'Extended · 1080p · H.264 · AAC · MP4']);
  });

  it('identical files are told apart by the differing token of their names, else a number', () => {
    const parts = versionNames([
      asset('h264_multi_audio_subs.mkv', '/m/Spirited Away (2001)/Spirited Away (2001) cd1.mkv'),
      asset('h264_multi_audio_subs.mkv', '/m/Spirited Away (2001)/Spirited Away (2001) cd2.mkv'),
    ]);
    expect(parts).toEqual(['1080p · H.264 · AAC · MKV · cd1', '1080p · H.264 · AAC · MKV · cd2']);
    const twins = versionNames([asset('h264_aac_faststart.mp4', '/a/Film.mp4'), asset('h264_aac_faststart.mp4', '/b/Film.mp4')]);
    expect(twins).toEqual(['1080p · H.264 · AAC · MP4 · 1', '1080p · H.264 · AAC · MP4 · 2']);
    // Windows paths compare by file name too, not by folder.
    const win = versionNames([
      asset('h264_multi_audio_subs.mkv', 'D:\\Media\\Spirited Away (2001)\\Spirited Away (2001) cd1.mkv'),
      asset('h264_multi_audio_subs.mkv', 'D:\\Media\\Spirited Away (2001)\\Spirited Away (2001) cd2.mkv'),
    ]);
    expect(win).toEqual(['1080p · H.264 · AAC · MKV · cd1', '1080p · H.264 · AAC · MKV · cd2']);
    const winTwins = versionNames([asset('h264_aac_faststart.mp4', 'D:\\a\\Film.mp4'), asset('h264_aac_faststart.mp4', 'D:\\b\\Film.mp4')]);
    expect(winTwins).toEqual(['1080p · H.264 · AAC · MP4 · 1', '1080p · H.264 · AAC · MP4 · 2']);
  });

  it('every title with several versions in the mock catalog gets distinct names', () => {
    const db = createMockDb(fx, Date.UTC(2026, 9, 18));
    let checked = 0;
    for (const id of db.libraryIds(1, 'title')) {
      const assets = db.detail(id)?.assets ?? [];
      if (assets.length < 2) continue;
      checked++;
      expect(new Set(versionNames(assets)).size, db.get(id)?.title).toBe(assets.length);
    }
    expect(checked).toBeGreaterThan(0);
  });
});

describe('subtitle files', () => {
  const file = (track: number, codec: 'subrip' | 'pgs', language: string | null, title: string | null) => ({
    track,
    path: `/m/Show/Show.S01E01.${track}`,
    codec,
    language,
    title,
    forced: false,
    hearing_impaired: false,
    size: 1,
    mtime_ns: 1,
  });

  it('labels a file by its title, language and format', () => {
    // Language names follow the runtime's locale.
    expect(subtitleFileLabel(file(1048577, 'pgs', 'chi', '中英字幕 某字幕组'))).toBe(`中英字幕 某字幕组 · ${languageName('chi')} · PGS`);
    expect(subtitleFileLabel({ ...file(1048578, 'subrip', 'eng', null), forced: true })).toBe(`${languageName('eng')} · SRT · forced`);
    expect(subtitleFileLabel(file(1048579, 'subrip', null, null))).toBe('Subtitles · SRT');
  });

  it('lists tracks, then files; picture formats are burned in', () => {
    const a = asset('h264_aac_faststart.mp4', '/m/Show/Show.S01E01.mkv');
    a.subtitle_files = [file(1048577, 'pgs', 'chi', null), file(1048578, 'subrip', 'eng', null)];
    const choices = subtitleChoices(a);
    const files = choices.slice(-2);
    expect(files.map((c) => [c.track, c.burnIn])).toEqual([
      [1048577, true],
      [1048578, false],
    ]);
    expect(subtitleChoices(undefined)).toEqual([]);
  });
});
