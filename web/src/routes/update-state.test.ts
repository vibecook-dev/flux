import { describe, expect, it } from 'vitest';
import type { UpdateInfo } from '../api/types';
import { checkedLine, downloaded, headline, mb, noteFor, stepLine, updating } from './update-state';

const info = (over: Partial<UpdateInfo> = {}): UpdateInfo => ({
  current: '0.1.0-alpha.4',
  check: true,
  check_fixed: false,
  checked_at: 1_000,
  available: { version: '0.1.0-alpha.5', published_at: null, url: 'https://x', highlights: [] },
  install: { kind: 'archive', can_apply: true, why_not: null, command: 'flux-server update' },
  phase: 'idle',
  progress: null,
  error: null,
  previous: null,
  ...over,
});

describe('update panel text', () => {
  it('says what is out, or that flux is up to date', () => {
    expect(headline(info())).toBe('flux 0.1.0-alpha.5 is out');
    expect(headline(info({ available: null }))).toBe('flux 0.1.0-alpha.4 is up to date');
  });

  it('says when flux last looked, and whether it looks by itself', () => {
    const ago = () => '3 hours ago';
    expect(checkedLine(info(), ago)).toBe('Checked 3 hours ago · checks daily');
    expect(checkedLine(info({ check: false }), ago)).toBe('Checked 3 hours ago · daily check off');
    expect(checkedLine(info({ checked_at: null }), ago)).toBe('Not checked yet · checks daily');
  });

  it('names each step of an update, with the download in megabytes', () => {
    expect(stepLine(info({ phase: 'downloading', progress: { done: 12_582_912, total: 25_185_736 } }))).toBe('Downloading 12.6 of 25.2 MB');
    expect(stepLine(info({ phase: 'downloading' }))).toBe('Downloading…');
    expect(stepLine(info({ phase: 'verifying' }))).toMatch(/trying the new version/);
    expect(stepLine(info({ phase: 'restarting' }))).toBe('Restarting flux…');
    expect(stepLine(info({ phase: 'idle' }))).toBeNull();
    expect(stepLine(info({ phase: 'failed' }))).toBeNull();
    expect(mb(0)).toBe('0.0');
  });

  it('measures the download, and only the download', () => {
    expect(downloaded(info({ phase: 'downloading', progress: { done: 50, total: 200 } }))).toBe(0.25);
    expect(downloaded(info({ phase: 'downloading', progress: { done: 300, total: 200 } }))).toBe(1);
    expect(downloaded(info({ phase: 'installing', progress: { done: 50, total: 200 } }))).toBeNull();
    expect(downloaded(info({ phase: 'downloading', progress: { done: 0, total: 0 } }))).toBeNull();
  });

  it('knows when an update is under way', () => {
    expect(['downloading', 'verifying', 'installing', 'restarting'].every((p) => updating(p as UpdateInfo['phase']))).toBe(true);
    expect(['idle', 'checking', 'failed'].some((p) => updating(p as UpdateInfo['phase']))).toBe(false);
  });
});

describe("the owner's note", () => {
  it('shows a release that is out until it is dismissed', () => {
    expect(noteFor(info(), null)).toBe('0.1.0-alpha.5');
    expect(noteFor(info(), '0.1.0-alpha.5')).toBeNull();
    // A newer one than the one dismissed shows again.
    expect(noteFor(info(), '0.1.0-alpha.4')).toBe('0.1.0-alpha.5');
  });

  it('stays away while nothing is out, nothing is known, or an update runs', () => {
    expect(noteFor(info({ available: null }), null)).toBeNull();
    expect(noteFor(undefined, null)).toBeNull();
    expect(noteFor(info({ phase: 'downloading' }), null)).toBeNull();
  });
});
