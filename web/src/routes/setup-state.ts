/**
 * What the setup screen (Setup.tsx) and the owner link (Own.tsx) say, kept
 * apart so it can be tested: device names, the folder browser's words, the
 * encoder flux transcodes with, how to get ffmpeg on each system, how to let
 * other devices in, and the address to show an owner link at.
 */
import { formatCount } from '../api/format';
import type { CodecChoice, FolderSummary, InstallKind, LibraryKind, OwnerLink } from '../api/types';

/** A name for this browser in the owner device list: "Safari on iPhone". */
export function deviceName(ua: string, touchPoints = 0): string {
  const browser = /Edg\//.test(ua)
    ? 'Edge'
    : /OPR\//.test(ua)
      ? 'Opera'
      : /Firefox\/|FxiOS\//.test(ua)
        ? 'Firefox'
        : /Chrome\/|CriOS\//.test(ua)
          ? 'Chrome'
          : /Safari\//.test(ua)
            ? 'Safari'
            : 'A browser';
  const os = /iPhone/.test(ua)
    ? 'iPhone'
    : /iPad/.test(ua) || (/Macintosh/.test(ua) && touchPoints > 1)
      ? 'iPad'
      : /Android/.test(ua)
        ? 'Android'
        : /Macintosh|Mac OS X/.test(ua)
          ? 'Mac'
          : /Windows/.test(ua)
            ? 'Windows'
            : /CrOS/.test(ua)
              ? 'Chromebook'
              : /Linux/.test(ua)
                ? 'Linux'
                : null;
  return os ? `${browser} on ${os}` : browser;
}

export const KIND_CHOICES: [LibraryKind, string][] = [
  ['movies', 'Films'],
  ['shows', 'TV shows'],
  ['mixed', 'Both'],
  ['videos', 'Home videos & clips'],
];

const KIND_WORDS: Record<LibraryKind, string> = { movies: 'films', shows: 'TV shows', mixed: 'films and shows', videos: 'home videos' };

/** "120 videos", "1,000+ videos", nothing when counting ran out of time. */
export function videosLabel(n: number | null, cap = 1000): string {
  if (n === null) return '';
  if (n === 0) return 'no videos';
  if (n >= cap) return `${formatCount(cap)}+ videos`;
  return n === 1 ? '1 video' : `${formatCount(n)} videos`;
}

/** What a folder holds, in a line: "532 videos, which look like TV shows". */
export function summaryLine(here: FolderSummary): string {
  if (here.videos === 0) return 'No videos in this folder (or the folders in it) yet.';
  const count = `${formatCount(here.videos)}${here.more ? '+' : ''} ${here.videos === 1 ? 'video' : 'videos'}`;
  return here.kind ? `${count}, which look like ${KIND_WORDS[here.kind]}.` : `${count}.`;
}

/** A library's name from its folder: "/Volumes/Media/Films" → "Films". */
export function nameFor(path: string): string {
  const parts = path.split(/[\\/]+/).filter(Boolean);
  const last = parts[parts.length - 1] ?? '';
  return /^[A-Za-z]:$/.test(last) || last === '' ? 'Library' : last;
}

/** The folders on the way to `path`, each with its own path, for a breadcrumb. */
export function crumbs(path: string): { name: string; path: string }[] {
  // A Windows share, \\server\share: one crumb for both.
  const share = /^[\\/]{2}[^\\/]+[\\/]+[^\\/]+/.exec(path);
  if (share) {
    const root = `${share[0].replace(/\//g, '\\')}\\`;
    const out = [{ name: root.slice(0, -1), path: root }];
    let at = root;
    for (const p of path.slice(share[0].length).split(/[\\/]+/).filter(Boolean)) {
      at = `${at}${at.endsWith('\\') ? '' : '\\'}${p}`;
      out.push({ name: p, path: at });
    }
    return out;
  }
  const windows = /^[A-Za-z]:[\\/]/.test(path) || path.includes('\\');
  const sep = windows ? '\\' : '/';
  const parts = path.split(/[\\/]+/).filter(Boolean);
  const out: { name: string; path: string }[] = [];
  let at = '';
  parts.forEach((p, i) => {
    at = windows ? (i === 0 ? `${p}${sep}` : `${at}${at.endsWith(sep) ? '' : sep}${p}`) : `${at}${sep}${p}`;
    out.push({ name: p, path: at });
  });
  if (!windows) out.unshift({ name: '/', path: '/' });
  return out;
}

const ENCODERS: Record<string, string> = {
  video_toolbox: 'VideoToolbox',
  nvenc: 'NVENC',
  qsv: 'Quick Sync',
  vaapi: 'VAAPI',
  libx264: 'libx264',
};

/** How flux transcodes video here: "VideoToolbox, on the GPU". */
export function encoderLine(video: CodecChoice[]): string {
  const h264 = video.find((c) => c.codec === 'h264') ?? video[0];
  if (!h264?.encoder) return 'No video encoder works, so flux plays only what your devices can play as it is.';
  const name = ENCODERS[h264.encoder] ?? h264.encoder;
  return h264.hardware ? `${name}, on the GPU` : `${name}, on the CPU (no hardware encoder found)`;
}

export interface FfmpegFix {
  /** A command to run, when there is one. */
  command: string | null;
  text: string;
  /** Where the full steps are. */
  link: string;
}

const INSTALL = 'https://github.com/vibecook-dev/flux/blob/main/docs/INSTALL.md';
/** How owner links work, in full. */
export const OWNER_LINK_DOCS = `${INSTALL}#the-owner-link`;

/** How to get ffmpeg on the server's system (`MediaTools.platform`). */
export function ffmpegFix(platform: string): FfmpegFix {
  if (platform.startsWith('macos')) {
    return { command: 'brew install ffmpeg', text: 'Install it with Homebrew, then restart flux.', link: `${INSTALL}#ffmpeg` };
  }
  if (platform.startsWith('windows')) {
    return {
      command: null,
      text: 'Download Jellyfin’s portable ffmpeg for Windows, unzip it, add its folder to PATH (or pass --ffmpeg), then restart flux.',
      link: `${INSTALL}#install`,
    };
  }
  return {
    command: 'sudo apt install ffmpeg',
    text: 'On Debian, Ubuntu or Raspberry Pi OS, install it with apt (Fedora: RPM Fusion’s ffmpeg), then restart flux.',
    link: `${INSTALL}#ffmpeg`,
  };
}

/** Where an install keeps its settings, so other devices can be let in. */
export interface LanSwitch {
  /** The settings file with `lan = "0.0.0.0:8097"` to uncomment. */
  file: string;
  /** Who may change it. */
  who: string | null;
  /** The command that restarts flux afterwards… */
  restart: string | null;
  /** …or the menu item that does (the macOS app's menu bar menu). */
  menu: string | null;
}

/** The settings file of a packaged install (the Windows installer, the .deb
 * and .rpm, the macOS app), which comes with `lan` commented out; null for
 * the others. */
export function lanSwitch(kind: InstallKind | undefined): LanSwitch | null {
  switch (kind) {
    case 'msi':
      return { file: 'C:\\ProgramData\\flux\\flux.toml', who: 'as administrator', restart: 'Restart-Service flux', menu: null };
    case 'deb':
    case 'rpm':
      return { file: '/etc/flux/flux.toml', who: null, restart: 'sudo systemctl restart flux', menu: null };
    case 'app':
      return { file: '~/Library/Application Support/flux/flux.toml', who: null, restart: null, menu: 'Restart flux' };
    default:
      return null;
  }
}

/** This machine's own address (`localhost`, 127.x, `[::1]`). */
export function isLoopback(host: string): boolean {
  return host === 'localhost' || host === '[::1]' || /^127\./.test(host);
}

/** On the tailnet, by name or by its addresses (100.64.0.0/10). */
export function isTailnet(host: string): boolean {
  const m = /^100\.(\d+)\./.exec(host);
  return host.endsWith('.ts.net') || (m !== null && Number(m[1]) >= 64 && Number(m[1]) < 128);
}

/**
 * The address to show the link at: the one this page is on when it is one
 * flux gives, else the network's (this page may be on localhost, or on the
 * tailnet, where every device has a login of its own and needs no link),
 * else this page's own address (a Docker host by name, say).
 */
export function linkUrl(link: OwnerLink, origin: string, hostname: string): string {
  const same = link.places.find((p) => p.url.startsWith(`${origin}/`));
  if (same) return same.url;
  const network = link.places.find((p) => p.place === 'network');
  if (network && (isLoopback(hostname) || isTailnet(hostname))) return network.url;
  if (!isLoopback(hostname) && !isTailnet(hostname)) return `${origin}${link.path}`;
  return link.places[0]?.url ?? `${origin}${link.path}`;
}
