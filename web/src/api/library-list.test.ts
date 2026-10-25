import { afterEach, describe, expect, it, vi } from 'vitest';
import type { ItemSummary, Page } from './types';

const libraryItems = vi.fn<(id: number, q: { cursor: string | null; limit: number }, signal?: AbortSignal) => Promise<Page<ItemSummary>>>();
vi.mock('./client', () => ({ api: { libraryItems: (...a: Parameters<typeof libraryItems>) => libraryItems(...a) } }));
vi.mock('./queries', () => ({ rememberSummaries: () => undefined }));

const { holdPaging, LibraryList } = await import('./library-list');

const item = (id: number) => ({ id, title: `T${id}` }) as ItemSummary;
const flush = () => new Promise((r) => setTimeout(r, 0));

afterEach(() => {
  holdPaging(false);
  libraryItems.mockReset();
});

describe('LibraryList paging', () => {
  it('loads the first page while held, and the rest once released', async () => {
    libraryItems.mockImplementation((_id, { cursor }) => {
      if (cursor === null) return Promise.resolve({ items: [item(1)], next: 'a', total: 3 });
      if (cursor === 'a') return Promise.resolve({ items: [item(2)], next: 'b', total: 3 });
      return Promise.resolve({ items: [item(3)], next: null, total: 3 });
    });
    holdPaging(true);
    const list = new LibraryList(1, 'title');
    list.load();
    await flush();
    await flush();
    expect(list.items.map((i) => i.id)).toEqual([1]);
    expect(list.total).toBe(3);
    expect(list.done).toBe(false);
    expect(libraryItems).toHaveBeenCalledTimes(1);

    holdPaging(false);
    await flush();
    await flush();
    await flush();
    expect(list.items.map((i) => i.id)).toEqual([1, 2, 3]);
    expect(list.done).toBe(true);
  });

  it('lets a held load be abandoned without fetching more', async () => {
    libraryItems.mockImplementation((_id, { cursor }) => Promise.resolve({ items: [item(cursor === null ? 1 : 2)], next: cursor === null ? 'a' : null, total: 2 }));
    holdPaging(true);
    const list = new LibraryList(2, 'title');
    list.load();
    await flush();
    list.dispose();
    holdPaging(false);
    await flush();
    expect(libraryItems).toHaveBeenCalledTimes(1);
  });
});
