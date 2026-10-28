/**
 * Downloads: the newest release (a snapshot taken at build time,
 * src/data/release.json, refreshed from GitHub's API in the browser), the
 * build for this visitor's system first, and the commands to install it.
 */
import snapshot from '../data/release.json';

interface Asset {
  name: string;
  size: number;
  url: string;
}

interface Release {
  tag: string;
  name: string;
  published_at: string;
  prerelease: boolean;
  url: string;
  assets: Asset[];
}

type System = 'mac-arm' | 'mac-intel' | 'windows' | 'linux-x64' | 'linux-arm' | 'docker';

interface Build {
  id: System;
  name: string;
  detail: string;
  target?: string;
  /** Its packages, best first: the first one the release has is offered. */
  exts?: string[];
  icon: string;
}

const REPO = 'vibecook-dev/flux';
const s = (d: string) => `<svg viewBox="0 0 24 24" aria-hidden="true">${d}</svg>`;
const ICONS = {
  laptop: s('<rect x="4.5" y="5.5" width="15" height="10" rx="1.6"/><path d="M2.5 18.5h19"/>'),
  window: s('<rect x="3.5" y="4.5" width="17" height="15" rx="2"/><path d="M3.5 9h17M12 9v10.5"/>'),
  terminal: s('<rect x="3.5" y="4.5" width="17" height="15" rx="2"/><path d="m7.5 10 2.5 2-2.5 2M12.5 14.5h4"/>'),
  box: s('<path d="M12 3.5 19.5 7.5v9L12 20.5 4.5 16.5v-9z"/><path d="M4.5 7.5 12 11.5l7.5-4M12 11.5v9" opacity=".6"/>'),
  code: s('<path d="m8.5 8-4 4 4 4M15.5 8l4 4-4 4M13.5 5.5l-3 13"/>'),
  check: s('<path d="M12 3.5 19 6.5v5.2c0 4.3-2.9 7.6-7 8.8-4.1-1.2-7-4.5-7-8.8V6.5z"/><path d="m8.8 12.2 2.2 2.2 4.3-4.6"/>'),
};

const BUILDS: Build[] = [
  { id: 'mac-arm', name: 'macOS', detail: 'Apple Silicon', target: 'aarch64-apple-darwin', exts: ['tar.gz'], icon: ICONS.laptop },
  { id: 'mac-intel', name: 'macOS', detail: 'Intel', target: 'x86_64-apple-darwin', exts: ['tar.gz'], icon: ICONS.laptop },
  // The installer (a service, FFmpeg included); releases before it, the zip.
  { id: 'windows', name: 'Windows', detail: 'x64', target: 'x86_64-pc-windows-msvc', exts: ['msi', 'zip'], icon: ICONS.window },
  { id: 'linux-x64', name: 'Linux', detail: 'x86_64', target: 'x86_64-unknown-linux-gnu', exts: ['tar.gz'], icon: ICONS.terminal },
  { id: 'linux-arm', name: 'Linux', detail: 'ARM64', target: 'aarch64-unknown-linux-gnu', exts: ['tar.gz'], icon: ICONS.terminal },
  { id: 'docker', name: 'Docker', detail: 'amd64 and arm64, with ffmpeg', icon: ICONS.box },
];

const version = (r: Release) => r.tag.replace(/^v/, '');
const filesOf = (r: Release, b: Build) => (b.target ? (b.exts ?? []).map((e) => `flux-${version(r)}-${b.target}.${e}`) : []);
const assetOf = (r: Release, b: Build) =>
  filesOf(r, b)
    .map((f) => r.assets.find((a) => a.name === f))
    .find((a) => a !== undefined) ?? null;
const fileOf = (r: Release, b: Build) => assetOf(r, b)?.name ?? filesOf(r, b)[0] ?? null;
const extOf = (file: string) => (file.endsWith('.tar.gz') ? 'tar.gz' : file.slice(file.lastIndexOf('.') + 1));
const mb = (n: number) => `${(n / 1_000_000).toFixed(1)} MB`;

/** A best guess at the visitor's system; Macs default to Apple Silicon, which Safari can't tell from Intel. */
async function detect(): Promise<System | null> {
  const nav = navigator as Navigator & {
    userAgentData?: { platform?: string; mobile?: boolean; getHighEntropyValues?: (h: string[]) => Promise<{ architecture?: string }> };
  };
  const ua = navigator.userAgent;
  if (/iPhone|iPad|iPod|Android/i.test(ua) || nav.userAgentData?.mobile) return null;
  const platform = nav.userAgentData?.platform ?? navigator.platform ?? '';
  let arch = '';
  try {
    arch = (await nav.userAgentData?.getHighEntropyValues?.(['architecture']))?.architecture ?? '';
  } catch {
    // Not offered: guess below.
  }
  if (/mac/i.test(platform) || /Mac OS X/.test(ua)) return arch === 'x86' ? 'mac-intel' : 'mac-arm';
  if (/win/i.test(platform)) return 'windows';
  if (/linux|x11|cros/i.test(platform + ua)) return arch === 'arm' || /aarch64|arm64/i.test(ua) ? 'linux-arm' : 'linux-x64';
  return null;
}

function install(r: Release, sys: System | null, tab: 'shell' | 'docker'): string {
  const v = version(r);
  const c = (t: string) => `<span class="c"># ${t}</span>`;
  const p = '<span class="p">$ </span>';
  if (tab === 'docker') {
    return [
      c('Your films stay read-only; flux keeps its catalog in the flux-config volume.'),
      `${p}docker run -d --name flux -p 8096:8096 \\`,
      `    -v flux-config:/config \\`,
      `    -v /path/to/movies:/media/movies:ro \\`,
      `    -e FLUX_LIBRARIES="Movies:movies:/media/movies" \\`,
      `    ghcr.io/${REPO}:${v}`,
      c('Then open http://localhost:8096'),
    ].join('\n');
  }
  if (sys === 'windows') {
    const zip = `flux-${v}-x86_64-pc-windows-msvc`;
    if (r.assets.some((a) => a.name === `${zip}.msi`)) {
      return [
        c('PowerShell: download and install; flux then runs as a service, FFmpeg included'),
        `<span class="p">&gt; </span>curl.exe -fLO https://github.com/${REPO}/releases/download/v${v}/${zip}.msi`,
        `<span class="p">&gt; </span>msiexec /i ${zip}.msi`,
        c('Then open http://localhost:8096 and set flux up'),
      ].join('\n');
    }
    return [
      c('PowerShell: download, unpack and point flux at your films'),
      `<span class="p">&gt; </span>curl.exe -fLO https://github.com/${REPO}/releases/download/v${v}/${zip}.zip`,
      `<span class="p">&gt; </span>Expand-Archive ${zip}.zip -DestinationPath .`,
      `<span class="p">&gt; </span>.\\${zip}\\flux-server.exe --library "Movies:movies:$HOME\\Videos"`,
      c('Then open http://127.0.0.1:8096'),
    ].join('\n');
  }
  const target = BUILDS.find((b) => b.id === sys)?.target ?? 'aarch64-apple-darwin';
  const dir = `flux-${v}-${target}`;
  return [
    c('Download, unpack and point flux at your films'),
    `${p}curl -fLO https://github.com/${REPO}/releases/download/v${v}/${dir}.tar.gz`,
    `${p}tar -xzf ${dir}.tar.gz`,
    `${p}./${dir}/bin/flux-server --library Movies:movies:$HOME/Movies`,
    c('Then open http://127.0.0.1:8096'),
  ].join('\n');
}

export async function startDownload(): Promise<void> {
  const root = document.querySelector<HTMLElement>('[data-get]');
  if (!root) return;
  const list = root.querySelector<HTMLElement>('[data-builds]')!;
  const code = root.querySelector<HTMLElement>('[data-install-code] code')!;
  const tabs = [...root.querySelectorAll<HTMLButtonElement>('[data-install-tab]')];
  const copyInstall = root.querySelector<HTMLButtonElement>('[data-copy-install]')!;
  let release = snapshot as Release;
  let sys: System | null = null;
  let tab: 'shell' | 'docker' = 'shell';

  const render = () => {
    const v = version(release);
    const date = new Date(release.published_at);
    root.querySelector('[data-rel-tag]')!.textContent = release.tag;
    root.querySelector('[data-rel-date]')!.textContent = `Released ${date.toLocaleDateString('en-GB', { day: 'numeric', month: 'short', year: 'numeric' })}`;
    root.querySelector<HTMLElement>('[data-rel-pre]')!.hidden = !release.prerelease;
    root.querySelector<HTMLAnchorElement>('[data-rel-notes]')!.href = release.url;

    const mine = BUILDS.find((b) => b.id === sys) ?? null;
    const asset = mine ? assetOf(release, mine) : null;
    const title = root.querySelector('[data-get-title]')!;
    const sub = root.querySelector('[data-get-sub]')!;
    const primary = root.querySelector<HTMLAnchorElement>('[data-get-primary]')!;
    const label = root.querySelector('[data-get-primary-label]')!;
    if (mine && asset) {
      title.textContent = `flux for ${mine.name}`;
      sub.textContent = `${mine.detail} · ${mb(asset.size)} · .${extOf(asset.name)}${mine.id === 'mac-arm' ? ' · on an Intel Mac, pick Intel below' : ''}`;
      primary.href = asset.url;
      label.textContent = `Download for ${mine.name}`;
    } else {
      title.textContent = 'flux for your computer';
      sub.textContent = 'flux runs on a Mac, a PC or a Linux box, and you watch from any device. Pick your system.';
      primary.href = release.url;
      label.textContent = 'All downloads';
    }

    // Every build, yours first.
    const order = [...BUILDS].sort((a, b) => Number(b.id === sys) - Number(a.id === sys));
    const rows = order.map((b) => {
      const a = assetOf(release, b);
      const href = b.id === 'docker' ? `https://github.com/${REPO}/pkgs/container/flux` : (a?.url ?? release.url);
      const file = b.id === 'docker' ? `ghcr.io/${REPO}:${v}` : (fileOf(release, b) ?? '');
      const size = a ? mb(a.size) : b.id === 'docker' ? 'image' : '';
      return `<li><a class="build" href="${href}"${b.id === sys ? ' data-yours' : ''}>
        <span class="build-icon">${b.icon}</span>
        <span class="build-text"><span class="build-name">${b.name} <span class="build-detail">${b.detail}</span>${b.id === sys ? '<small>Yours</small>' : ''}</span><span class="build-file">${file}</span></span>
        <span class="build-size">${size}</span></a></li>`;
    });
    rows.push(`<li><a class="build" href="https://github.com/${REPO}/archive/refs/tags/${release.tag}.tar.gz">
      <span class="build-icon">${ICONS.code}</span>
      <span class="build-text"><span class="build-name">Source code <span class="build-detail">Rust and TypeScript</span></span><span class="build-file">${release.tag}.tar.gz</span></span>
      <span class="build-size"></span></a></li>`);
    const sums = release.assets.find((a) => a.name === 'SHA256SUMS');
    if (sums) {
      rows.push(`<li><a class="build" href="${sums.url}">
        <span class="build-icon">${ICONS.check}</span>
        <span class="build-text"><span class="build-name">Checksums <span class="build-detail">for every package</span></span><span class="build-file">SHA256SUMS</span></span>
        <span class="build-size"></span></a></li>`);
    }
    list.innerHTML = rows.join('');
    code.innerHTML = install(release, sys, tab);
  };

  for (const t of tabs) {
    t.addEventListener('click', () => {
      tab = t.dataset.installTab as 'shell' | 'docker';
      tabs.forEach((x) => x.setAttribute('aria-selected', String(x === t)));
      code.innerHTML = install(release, sys, tab);
    });
  }
  copyInstall.addEventListener('click', () => {
    // Copy the commands without the prompts and comments.
    const text = [...code.textContent!.split('\n')]
      .filter((l) => !l.trim().startsWith('#'))
      .map((l) => l.replace(/^(\$|>) /, ''))
      .join('\n');
    void copy(copyInstall, text);
  });

  render();
  sys = await detect();
  render();
  // The OS in the hero's button.
  const mine = BUILDS.find((b) => b.id === sys);
  document.querySelectorAll<HTMLElement>('[data-os-cta]').forEach((el) => (el.textContent = mine ? `Download for ${mine.name}` : 'Download'));

  // Fresher than the snapshot? GitHub's API answers this page directly (60 requests an hour per visitor).
  try {
    const cached = sessionStorage.getItem('flux.site.release');
    const fresh = cached ? (JSON.parse(cached) as { at: number; r: Release }) : null;
    let latest = fresh && Date.now() - fresh.at < 30 * 60_000 ? fresh.r : null;
    if (!latest) {
      const res = await fetch(`https://api.github.com/repos/${REPO}/releases?per_page=5`, { headers: { accept: 'application/vnd.github+json' } });
      if (!res.ok) return;
      const all = (await res.json()) as {
        tag_name: string;
        name: string;
        published_at: string;
        prerelease: boolean;
        draft: boolean;
        html_url: string;
        assets: { name: string; size: number; browser_download_url: string }[];
      }[];
      const r = all.find((x) => !x.draft);
      if (!r) return;
      latest = {
        tag: r.tag_name,
        name: r.name,
        published_at: r.published_at,
        prerelease: r.prerelease,
        url: r.html_url,
        assets: r.assets.map((a) => ({ name: a.name, size: a.size, url: a.browser_download_url })),
      };
      sessionStorage.setItem('flux.site.release', JSON.stringify({ at: Date.now(), r: latest }));
    }
    if (latest.tag !== release.tag && new Date(latest.published_at) > new Date(release.published_at)) {
      release = latest;
      render();
    }
  } catch {
    // Offline or rate-limited: the snapshot stands.
  }
}

export async function copy(button: HTMLElement, text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    const was = button.textContent;
    button.textContent = 'Copied';
    button.setAttribute('data-copied', '');
    setTimeout(() => {
      button.textContent = was;
      button.removeAttribute('data-copied');
    }, 1600);
  } catch {
    // No clipboard here: select the text instead.
    const range = document.createRange();
    const target = button.parentElement?.querySelector('code');
    if (target) {
      range.selectNodeContents(target);
      const sel = getSelection();
      sel?.removeAllRanges();
      sel?.addRange(range);
    }
  }
}
