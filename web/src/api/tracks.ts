/** Human labels for probed tracks: pickers, the files panel and the stats overlay. */
import { channelLayout, codecName, codecShort, containerName, hdrName } from './explain';
import type { AssetInfo, Codec, ProbeResult, SubtitleFile, Track } from './types';

// ISO 639-2 (as found in Matroska/MP4) → BCP 47 for Intl.DisplayNames.
const ISO639_2: Record<string, string> = {
  eng: 'en', jpn: 'ja', fra: 'fr', fre: 'fr', deu: 'de', ger: 'de', spa: 'es', ita: 'it', kor: 'ko', zho: 'zh',
  chi: 'zh', rus: 'ru', por: 'pt', nld: 'nl', dut: 'nl', swe: 'sv', nor: 'no', dan: 'da', fin: 'fi', pol: 'pl',
  hin: 'hi', ara: 'ar', tur: 'tr', ces: 'cs', cze: 'cs', hun: 'hu', ell: 'el', gre: 'el', heb: 'he', tha: 'th',
};

let names: Intl.DisplayNames | null = null;

/** "eng" → "en", "jpn" → "ja"; BCP 47 tags pass through. */
export function toBcp47(code: string | null): string | undefined {
  if (!code || code === 'und') return undefined;
  return ISO639_2[code.toLowerCase()] ?? code;
}

export function languageName(code: string | null): string | null {
  if (!code || code === 'und') return null;
  const tag = ISO639_2[code.toLowerCase()] ?? code;
  try {
    names ??= new Intl.DisplayNames(undefined, { type: 'language' });
    return names.of(tag) ?? code;
  } catch {
    return code;
  }
}

/** "English · TrueHD 7.1 · Atmos" */
export function audioLabel(t: Track): string {
  const lang = languageName(t.language);
  const parts = [lang ?? `Track ${t.index + 1}`, `${codecShort(t.codec)} ${channelLayout(t.channels)}`.trim()];
  if (t.title && t.title !== lang && !parts[1].includes(t.title)) parts.push(t.title);
  return parts.join(' · ');
}

/** "English (Signs & Songs) · ASS" */
export function subtitleLabel(t: Track): string {
  const lang = languageName(t.language) ?? `Track ${t.index + 1}`;
  const title = t.title && t.title !== lang ? t.title : lang;
  return `${title} · ${codecShort(t.codec)}${t.forced ? ' · forced' : ''}${t.hearing_impaired ? ' · SDH' : ''}`;
}

/** Picture subtitle formats: shown by drawing them into the video (a transcode). */
export function isImageSubtitle(codec: Codec): boolean {
  return codec === 'pgs' || codec === 'vob_sub' || codec === 'dvb_sub';
}

/** "中英双语字幕 · Chinese · PGS", "English · SRT · forced": a subtitle file beside the video. */
export function subtitleFileLabel(f: SubtitleFile): string {
  const lang = languageName(f.language);
  const parts = [f.title ?? lang ?? 'Subtitles'];
  if (f.title && lang) parts.push(lang);
  parts.push(codecShort(f.codec));
  if (f.forced) parts.push('forced');
  if (f.hearing_impaired) parts.push('SDH');
  return parts.join(' · ');
}

/** A subtitle choice for the pickers: a track of the file or a subtitle file beside it. */
export interface SubtitleChoice {
  track: number;
  label: string;
  /** Drawn into the video (a picture format): picking it restarts playback as a transcode. */
  burnIn: boolean;
}

/** Every subtitle of a version: its tracks, then the subtitle files beside it. */
export function subtitleChoices(a: AssetInfo | undefined): SubtitleChoice[] {
  const tracks = (a?.probe?.tracks ?? []).filter((t) => t.kind === 'subtitle');
  return [
    ...tracks.map((t) => ({ track: t.index, label: subtitleLabel(t), burnIn: isImageSubtitle(t.codec) })),
    ...(a?.subtitle_files ?? []).map((f) => ({ track: f.track, label: subtitleFileLabel(f), burnIn: isImageSubtitle(f.codec) })),
  ];
}

/** "3840×2160 · HEVC Main 10 · HDR10" */
export function videoLabel(t: Track): string {
  const parts: string[] = [];
  if (t.width && t.height) parts.push(`${t.width}×${t.height}`);
  parts.push(`${codecName(t.codec)}${t.profile ? ` ${t.profile}` : ''}`);
  if (t.hdr && t.hdr.format !== 'sdr') parts.push(hdrName(t.hdr.format));
  if (t.bit_depth && t.bit_depth > 8) parts.push(`${t.bit_depth}-bit`);
  return parts.join(' · ');
}

export function resolutionName(width: number | null): string {
  if (!width) return '';
  if (width >= 3800) return '2160p';
  if (width >= 1900) return '1080p';
  if (width >= 1270) return '720p';
  if (width >= 700) return '480p';
  return 'SD';
}

/** What tells versions of one title apart, most telling first: "2160p", "HEVC", "HDR10", "E-AC-3", "MKV". */
function versionParts(a: AssetInfo): string[] {
  const tracks = a.probe?.tracks ?? [];
  const v = tracks.find((t) => t.kind === 'video');
  const audio = tracks.filter((t) => t.kind === 'audio');
  const au = audio.find((t) => t.default) ?? audio[0];
  return [
    v ? resolutionName(v.width) : '',
    v ? codecShort(v.codec) : '',
    v?.hdr && v.hdr.format !== 'sdr' ? hdrName(v.hdr.format) : '',
    au ? codecShort(au.codec) : '',
    containerName(a.container),
  ].filter(Boolean);
}

const SEPARATOR = /[\s._()[\]-]/;

/** The token of each file name that differs ("cd1" / "cd2"), or null when there is none worth showing. */
function nameDifferences(paths: string[]): (string | null)[] {
  const names = paths.map((p) => (p.split('/').pop() ?? p).replace(/\.[^.]+$/, ''));
  const first = names[0];
  let pre = 0;
  while (names.every((n) => pre < n.length && n[pre] === first[pre])) pre++;
  let suf = 0;
  while (names.every((n) => suf < n.length - pre && suf < first.length - pre && n[n.length - 1 - suf] === first[first.length - 1 - suf])) suf++;
  // Widen to whole tokens: "cd1" / "cd2", not "1" / "2".
  while (pre > 0 && !SEPARATOR.test(first[pre - 1])) pre--;
  while (suf > 0 && !SEPARATOR.test(first[first.length - suf])) suf--;
  return names.map((n) => {
    const mid = n.slice(pre, n.length - suf).replace(/^[\s._()[\]-]+|[\s._()[\]-]+$/g, '');
    return mid && mid.length <= 24 ? mid : null;
  });
}

/** "Director's Cut", else the resolution. */
function shortVersionName(a: AssetInfo): string {
  if (a.edition) return a.edition;
  const v = a.probe?.tracks.find((t) => t.kind === 'video');
  return (v && resolutionName(v.width)) || 'Version';
}

/**
 * Names for the version picker that tell every version apart. A version is
 * named by its edition ("Director's Cut") or its resolution; when two would
 * share a name (two 2160p files), both get the full "2160p · HEVC · E-AC-3 ·
 * MKV". Files that still look the same get the token of their file names that
 * differs ("cd1", "cd2"), or a number.
 */
export function versionNames(assets: AssetInfo[]): string[] {
  const short = assets.map(shortVersionName);
  const named = assets.map((a, i) => {
    if (short.filter((s) => s === short[i]).length < 2) return short[i];
    return [...(a.edition ? [a.edition] : []), ...versionParts(a)].join(' · ');
  });
  const out = [...named];
  for (const name of new Set(named)) {
    const twins = named.flatMap((n, i) => (n === name ? [i] : []));
    if (twins.length < 2) continue;
    const diffs = nameDifferences(twins.map((i) => assets[i].path));
    const usable = diffs.every((d) => d !== null) && new Set(diffs).size === diffs.length;
    twins.forEach((i, k) => (out[i] = `${name} · ${usable ? diffs[k] : String(k + 1)}`));
  }
  return out;
}

/** The edge-code facts for a version: ["2160P", "HEVC MAIN 10", "HDR10", "E-AC-3 5.1", "MKV"]. */
export function assetFacts(a: AssetInfo, audioIndex?: number | null): string[] {
  const p: ProbeResult | null = a.probe;
  const v = p?.tracks.find((t) => t.kind === 'video');
  const audio = p?.tracks.filter((t) => t.kind === 'audio') ?? [];
  const au = audio.find((t) => t.index === audioIndex) ?? audio.find((t) => t.default) ?? audio[0];
  const out: string[] = [];
  if (v) {
    out.push(resolutionName(v.width));
    out.push(`${codecShort(v.codec)}${v.profile && v.codec !== 'h264' ? ` ${v.profile}` : ''}`);
    if (v.hdr && v.hdr.format !== 'sdr') out.push(hdrName(v.hdr.format));
  }
  if (au) out.push(`${codecShort(au.codec)} ${channelLayout(au.channels)}`.trim());
  return out.filter(Boolean);
}
