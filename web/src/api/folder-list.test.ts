import { afterEach, describe, expect, it, vi } from 'vitest';
import { api } from './client';
import { getFolderList, refreshFolders } from './folder-list';
import type { FolderPage } from './types';

describe('cached folder lists', () => {
  afterEach(() => vi.restoreAllMocks());

  it('reload on refreshFolders, for one library or for all', async () => {
    let served = 0;
    const spy = vi.spyOn(api, 'libraryFolders').mockImplementation((): Promise<FolderPage> => {
      served += 1;
      return Promise.resolve({ at: { folder: '', name: 'Home' }, parents: [], folders: [], items: [], next: null, total: served });
    });
    const a = getFolderList(901, '', 'title');
    const b = getFolderList(902, '', 'title');
    await vi.waitFor(() => expect(a.done && b.done).toBe(true));
    expect(spy).toHaveBeenCalledTimes(2);
    // Back to a folder: the cached list, as it was.
    expect(getFolderList(901, '', 'title')).toBe(a);
    expect(spy).toHaveBeenCalledTimes(2);

    // A watched toggle reloads every library's folders…
    const total = a.total;
    refreshFolders();
    await vi.waitFor(() => expect(spy).toHaveBeenCalledTimes(4));
    await vi.waitFor(() => expect(a.done && b.done).toBe(true));
    expect(a.total).not.toBe(total);
    // …and a scan its own library's.
    refreshFolders(902);
    await vi.waitFor(() => expect(spy).toHaveBeenCalledTimes(5));
    expect(spy.mock.calls.at(-1)?.[0]).toBe(902);
  });
});
