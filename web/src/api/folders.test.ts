import { describe, expect, it } from 'vitest';
import { mockFolderPage, type MockFolderFile } from '../mock/folders';
import { breadcrumbs, folderHref, parseFolderSort } from './folders';

describe('folder view helpers', () => {
  it('links a folder with its sort, the top without a query', () => {
    expect(folderHref(4, '')).toBe('/library/4');
    expect(folderHref(4, '', '-date')).toBe('/library/4?fsort=-date');
    expect(folderHref(4, '3/2019/Day 2', 'title')).toBe('/library/4?folder=3%2F2019%2FDay+2');
  });

  it('reads unknown sorts as name order', () => {
    expect(parseFolderSort(null)).toBe('title');
    expect(parseFolderSort('newest')).toBe('title');
    expect(parseFolderSort('-date')).toBe('-date');
  });

  it('marks the folder on screen as the last crumb', () => {
    const crumbs = breadcrumbs(
      [
        { folder: '', name: 'Home' },
        { folder: '3/2019', name: '2019' },
      ],
      { folder: '3/2019/Trip', name: 'Trip' },
    );
    expect(crumbs.map((c) => [c.ref.name, c.current])).toEqual([
      ['Home', false],
      ['2019', false],
      ['Trip', true],
    ]);
    expect(breadcrumbs([], null)).toEqual([]);
  });
});

describe('mock folder view', () => {
  const files: MockFolderFile[] = [
    { id: 1, rel: 'a.mp4', mtime: 50, sortKey: 'a' },
    { id: 2, rel: 'Trip/b.mp4', mtime: 30, sortKey: 'b' },
    { id: 3, rel: 'Trip/c.mp4', mtime: 10, sortKey: 'c' },
    { id: 4, rel: 'Trip/Day 10/e.mp4', mtime: 40, sortKey: 'e' },
    { id: 5, rel: 'Trip/Day 2/d.mp4', mtime: 20, sortKey: 'd' },
  ];
  const lib = { name: 'Home', root: 7 };

  it('lists subfolders in natural order and the videos directly here', () => {
    const top = mockFolderPage(lib, files, '', 'title', 0, 100);
    expect(top?.folders).toEqual([{ folder: '7/Trip', name: 'Trip', item_count: 4 }]);
    expect(top?.ids).toEqual([1]);
    const trip = mockFolderPage(lib, files, '7/Trip', 'title', 0, 100);
    expect(trip?.folders.map((f) => f.name)).toEqual(['Day 2', 'Day 10']);
    expect(trip?.parents).toEqual([{ folder: '', name: 'Home' }]);
    expect(trip?.at).toEqual({ folder: '7/Trip', name: 'Trip' });
    expect(mockFolderPage(lib, files, '7/Trip', '-date', 0, 100)?.ids).toEqual([2, 3]);
    expect(mockFolderPage(lib, files, '7/Trip', 'date', 0, 1)).toMatchObject({ ids: [3], next: 'o1', total: 2 });
    expect(mockFolderPage(lib, files, '7/Nope', 'title', 0, 100)).toBeNull();
    expect(mockFolderPage(lib, files, '8/Trip', 'title', 0, 100)).toBeNull();
  });
});
