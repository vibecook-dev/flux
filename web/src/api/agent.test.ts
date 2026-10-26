import { describe, expect, it } from 'vitest';
import { fieldName, fixText, valueText, type FileFix } from './agent';
import { collectionOf } from './space-list';

const fix = (f: Partial<FileFix>): FileFix => ({
  kind: null,
  of: null,
  title: null,
  year: null,
  season: null,
  episodes: null,
  extra: null,
  edition: null,
  part: null,
  ...f,
});

describe('the agent contract, as people read it', () => {
  it('says what a file was fixed to be', () => {
    expect(fixText(null)).toBe('what its name says');
    expect(fixText(fix({ kind: 'ignore' }))).toBe('not part of the library');
    expect(fixText(fix({ season: 2, episodes: [5] }))).toBe('S02E05');
    expect(fixText(fix({ season: 1, episodes: [1, 2] }))).toBe('S01E01-E02');
    expect(fixText(fix({ kind: 'extra', extra: 'trailer', of: 40 }))).toBe('a trailer · of item 40');
    expect(fixText(fix({ title: 'The Thing', year: 1982 }))).toBe('“The Thing” · 1982');
  });

  it('names the steps of a file plan', () => {
    expect(fieldName('file.path')).toBe('File');
    expect(fieldName('folder.new')).toBe('New folder');
    expect(fieldName('file.fix')).toBe('What the file is');
    expect(valueText(['ja', 'en'], 'languages')).toBe('Japanese, English');
  });

  it('tells a collection space from a library', () => {
    expect(collectionOf('c3')).toBe(3);
    expect(collectionOf(3)).toBeNull();
    expect(collectionOf('all')).toBeNull();
  });
});
