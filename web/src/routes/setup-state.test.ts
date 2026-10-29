import { describe, expect, it } from 'vitest';
import type { OwnerLink } from '../api/types';
import { crumbs, deviceName, encoderLine, ffmpegFix, isLoopback, isTailnet, lanSwitch, linkUrl, nameFor, summaryLine, videosLabel } from './setup-state';

describe('deviceName', () => {
  it('names the browser and the system it runs on', () => {
    expect(deviceName('Mozilla/5.0 (iPhone; CPU iPhone OS 26_1 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.1 Mobile/15E148 Safari/604.1')).toBe('Safari on iPhone');
    expect(deviceName('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.1 Safari/605.1.15', 5)).toBe('Safari on iPad');
    expect(deviceName('Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36')).toBe('Chrome on Mac');
    expect(deviceName('Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Safari/537.36 Edg/142.0.0.0')).toBe('Edge on Windows');
    expect(deviceName('Mozilla/5.0 (Linux; Android 16; Pixel 9) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/142.0.0.0 Mobile Safari/537.36')).toBe('Chrome on Android');
    expect(deviceName('Mozilla/5.0 (X11; Linux x86_64; rv:145.0) Gecko/20100101 Firefox/145.0')).toBe('Firefox on Linux');
    expect(deviceName('curl/8.0')).toBe('A browser');
  });
});

describe('the folder browser', () => {
  it('counts videos the way it can', () => {
    expect(videosLabel(null)).toBe('');
    expect(videosLabel(0)).toBe('no videos');
    expect(videosLabel(1)).toBe('1 video');
    expect(videosLabel(532)).toBe('532 videos');
    expect(videosLabel(1000)).toMatch(/^1.000\+ videos$/);
  });

  it('says what a folder holds', () => {
    const here = { videos: 532, more: false, films: 2, episodes: 520, kind: 'shows' as const, taken_by: null };
    expect(summaryLine(here)).toBe('532 videos, which look like TV shows.');
    expect(summaryLine({ ...here, videos: 600, more: true, kind: 'mixed' })).toBe('600+ videos, which look like films and shows.');
    expect(summaryLine({ ...here, videos: 0, kind: null })).toMatch(/^No videos/);
  });

  it('names a library after its folder', () => {
    expect(nameFor('/Volumes/Media/Films')).toBe('Films');
    expect(nameFor('/srv/media/TV/')).toBe('TV');
    expect(nameFor('D:\\Media\\Home Videos')).toBe('Home Videos');
    expect(nameFor('D:\\')).toBe('Library');
    expect(nameFor('/')).toBe('Library');
  });

  it('breaks a path into the folders on the way', () => {
    expect(crumbs('/Volumes/Media/Films')).toEqual([
      { name: '/', path: '/' },
      { name: 'Volumes', path: '/Volumes' },
      { name: 'Media', path: '/Volumes/Media' },
      { name: 'Films', path: '/Volumes/Media/Films' },
    ]);
    expect(crumbs('D:\\Media\\TV')).toEqual([
      { name: 'D:', path: 'D:\\' },
      { name: 'Media', path: 'D:\\Media' },
      { name: 'TV', path: 'D:\\Media\\TV' },
    ]);
    expect(crumbs('/')).toEqual([{ name: '/', path: '/' }]);
    // A share is one place: its crumbs keep the \\server\share root.
    expect(crumbs('\\\\nas\\media\\Films')).toEqual([
      { name: '\\\\nas\\media', path: '\\\\nas\\media\\' },
      { name: 'Films', path: '\\\\nas\\media\\Films' },
    ]);
  });
});

describe('owner links', () => {
  const link: OwnerLink = {
    path: '/own#T',
    expires_at: 0,
    places: [
      { url: 'http://192.168.1.20:8097/own#T', place: 'network' },
      { url: 'http://10.0.0.5:8097/own#T', place: 'network' },
    ],
  };
  it('shows the link at an address the device can open', () => {
    // This page's own address, when flux gives it.
    expect(linkUrl(link, 'http://10.0.0.5:8097', '10.0.0.5')).toBe('http://10.0.0.5:8097/own#T');
    // On localhost or the tailnet: the network's.
    expect(linkUrl(link, 'http://localhost:8096', 'localhost')).toBe('http://192.168.1.20:8097/own#T');
    expect(linkUrl(link, 'https://studio.tail1234.ts.net:8096', 'studio.tail1234.ts.net')).toBe('http://192.168.1.20:8097/own#T');
    // By a name flux doesn't list (nas.local, a Docker host): this page's.
    expect(linkUrl(link, 'http://nas.local:8096', 'nas.local')).toBe('http://nas.local:8096/own#T');
    // Docker, opened on the host's localhost.
    const docker: OwnerLink = { ...link, places: [{ url: 'http://localhost:8096/own#T', place: 'docker_host' }] };
    expect(linkUrl(docker, 'http://localhost:8096', 'localhost')).toBe('http://localhost:8096/own#T');
    // Nowhere else: this page's.
    expect(linkUrl({ ...link, places: [] }, 'http://localhost:8096', 'localhost')).toBe('http://localhost:8096/own#T');
  });
  it('knows this machine and the tailnet by their addresses', () => {
    expect(['localhost', '127.0.0.1', '[::1]'].every(isLoopback)).toBe(true);
    expect(isLoopback('192.168.1.20')).toBe(false);
    expect(['100.64.0.7', '100.127.1.1', 'a.tail1.ts.net'].every(isTailnet)).toBe(true);
    expect(['100.63.0.1', '100.128.0.1', '10.0.0.5'].some(isTailnet)).toBe(false);
  });
});

describe('playback', () => {
  it('says how flux transcodes', () => {
    expect(encoderLine([{ codec: 'h264', encoder: 'video_toolbox', hardware: true, tonemap: 'gpu' }])).toBe('VideoToolbox, on the GPU');
    expect(encoderLine([{ codec: 'h264', encoder: 'libx264', hardware: false, tonemap: null }])).toMatch(/^libx264, on the CPU/);
    expect(encoderLine([{ codec: 'h264', encoder: null, hardware: false, tonemap: null }])).toMatch(/^No video encoder works/);
    expect(encoderLine([])).toMatch(/^No video encoder works/);
  });

  it('knows where each system gets ffmpeg', () => {
    expect(ffmpegFix('macos aarch64').command).toBe('brew install ffmpeg');
    expect(ffmpegFix('linux x86_64').command).toBe('sudo apt install ffmpeg');
    const win = ffmpegFix('windows x86_64');
    expect(win.command).toBeNull();
    expect(win.text).toMatch(/Jellyfin/);
  });
});

describe('other devices', () => {
  it('names the settings file of a packaged install', () => {
    expect(lanSwitch('msi')).toEqual({ file: 'C:\\ProgramData\\flux\\flux.toml', who: 'as administrator', restart: 'Restart-Service flux', menu: null });
    expect(lanSwitch('deb')?.file).toBe('/etc/flux/flux.toml');
    expect(lanSwitch('rpm')?.restart).toBe('sudo systemctl restart flux');
    expect(lanSwitch('app')).toEqual({ file: '~/Library/Application Support/flux/flux.toml', who: null, restart: null, menu: 'Restart flux' });
    expect(lanSwitch('archive')).toBeNull();
    expect(lanSwitch('homebrew')).toBeNull();
    expect(lanSwitch(undefined)).toBeNull();
  });
});
