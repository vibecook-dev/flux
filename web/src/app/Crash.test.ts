// @vitest-environment jsdom
import { act, createElement } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const recover = vi.hoisted(() => vi.fn());
vi.mock('./update-checks', () => ({ recoverFromStaleCode: recover }));

const { CrashBoundary } = await import('./Crash');

(globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;

/** React's act for a change and whatever it starts (renders, effects, the promises they wait on). */
const settled = (change: () => void) =>
  act(async () => {
    change();
    await Promise.resolve();
  });

function Throws({ error }: { error: Error }): never {
  throw error;
}

let host: HTMLDivElement;
let root: Root;

beforeEach(() => {
  host = document.createElement('div');
  document.body.append(host);
  root = createRoot(host);
  // React reports every error a boundary catches.
  vi.spyOn(console, 'error').mockImplementation(() => undefined);
});

afterEach(() => {
  act(() => root.unmount());
  host.remove();
  vi.restoreAllMocks();
  recover.mockReset();
});

const show = async (place: 'app' | 'overlay' | 'player', error: Error, extra: Record<string, unknown> = {}) => {
  await settled(() => root.render(createElement(CrashBoundary, { place, ...extra, children: createElement(Throws, { error }) })));
};

describe('CrashBoundary', () => {
  it('shows what it holds while nothing fails', async () => {
    await settled(() => root.render(createElement(CrashBoundary, { place: 'app', children: createElement('p', null, 'the library') })));
    expect(host.textContent).toBe('the library');
  });

  it('a screen whose code is gone: flux was updated, then what came of reloading', async () => {
    let settle: (r: string) => void = () => undefined;
    recover.mockReturnValue(new Promise((r) => (settle = r)));
    await show('overlay', new TypeError('Failed to fetch dynamically imported module: https://x/assets/Connect-BM4E1TBp.js'));
    expect(host.querySelector('[role="alert"]')?.textContent).toContain('flux was updated');
    expect(host.textContent).toContain('Opening the new version');
    expect(recover).toHaveBeenCalledOnce();
    await settled(() => settle('offline'));
    expect(host.textContent).toContain('Can’t reach flux');
    expect([...host.querySelectorAll('button')].map((b) => b.textContent)).toEqual(['Try again']);
  });

  it('code that didn’t arrive though flux wasn’t updated: reload, or close the page', async () => {
    recover.mockResolvedValue('stuck');
    const onClose = vi.fn();
    await show('overlay', new TypeError('Importing a module script failed.'), { onClose });
    expect(host.textContent).toContain('This screen didn’t load');
    const buttons = [...host.querySelectorAll('button')];
    expect(buttons.map((b) => b.textContent)).toEqual(['Reload', 'Close']);
    await settled(() => buttons[1].click());
    expect(onClose).toHaveBeenCalledOnce();
  });

  it('any other error: says so, with the details folded away, and doesn’t go looking for updates', async () => {
    const onBack = vi.fn();
    await show('player', new TypeError('Cannot read properties of undefined (reading "streams")'), { onBack });
    expect(host.textContent).toContain('Something went wrong');
    expect(host.querySelector('details code')?.textContent).toContain('reading "streams"');
    expect(recover).not.toHaveBeenCalled();
    const back = [...host.querySelectorAll('button')].find((b) => b.textContent === 'Back');
    await settled(() => back?.click());
    expect(onBack).toHaveBeenCalledOnce();
  });
});
