import { describe, expect, it } from 'vitest';

import {
  DEFAULT_SORT,
  filterFields,
  formatCell,
  listColumns,
  listStateParams,
  nextSort,
  parseListState,
  sortText,
} from './listing';
import type { DocTypeMeta, FieldMeta, FieldType } from './types';

const field = (name: string, type: FieldType, extra: Partial<FieldMeta> = {}): FieldMeta => ({
  name,
  label: name.toUpperCase(),
  type,
  required: false,
  unique: false,
  allow_on_submit: false,
  ...extra,
});

const meta = (fields: FieldMeta[], submittable = false): DocTypeMeta => ({
  name: 'thing',
  label: 'Thing',
  naming: 'random',
  submittable,
  fields,
  rights: {
    read: 'all', write: 'all', create: 'all', delete: 'all', submit: 'none', cancel: 'none', amend: 'none',
  },
});

describe('listColumns', () => {
  it('is name, at most five fields without Table and Text, then modified_at', () => {
    const columns = listColumns(
      meta([
        field('a', 'data'),
        field('notes', 'text'),
        field('lines', 'table'),
        field('b', 'int'),
        field('c', 'link'),
        field('d', 'check'),
        field('e', 'date'),
        field('f', 'select'),
        field('g', 'decimal'),
      ]),
    );
    expect(columns.map((c) => c.key)).toEqual(['name', 'a', 'b', 'c', 'd', 'e', 'modified_at']);
    expect(columns[1]?.label).toBe('A');
    expect(columns.at(-1)).toMatchObject({ key: 'modified_at', type: 'datetime' });
  });

  it('works for a DocType with no fields', () => {
    expect(listColumns(meta([])).map((c) => c.key)).toEqual(['name', 'modified_at']);
  });
});

describe('filterFields', () => {
  it('has the standard columns and every field but Table fields', () => {
    const names = (m: DocTypeMeta) => filterFields(m).map((f) => f.name);
    const base = meta([field('a', 'data'), field('lines', 'table'), field('t', 'select', { options: ['x'] })]);
    expect(names(base)).toEqual(['name', 'owner', 'modified_at', 'a', 't']);
    expect(filterFields(base).at(-1)?.options).toEqual(['x']);
    expect(names({ ...base, submittable: true })).toContain('docstatus');
    expect(names(base)).not.toContain('docstatus');
  });
});

describe('parseListState and listStateParams', () => {
  const sortable = ['name', 'a', 'modified_at'];

  it('has defaults', () => {
    expect(parseListState(new URLSearchParams(), sortable)).toEqual({
      offset: 0,
      sort: DEFAULT_SORT,
      filters: null,
    });
    expect(listStateParams({ offset: 0, sort: DEFAULT_SORT, filters: null }).toString()).toBe('');
  });

  it('round-trips offset, sort and filters', () => {
    const state = { offset: 40, sort: { field: 'a', desc: true }, filters: '[{"field":"a","op":"is null"}]' };
    const params = listStateParams(state);
    expect(params.get('sort')).toBe('a:desc');
    expect(params.get('offset')).toBe('40');
    expect(parseListState(new URLSearchParams(params.toString()), sortable)).toEqual(state);
  });

  it('ignores bad values', () => {
    for (const query of ['offset=-1', 'offset=abc', 'offset=1.5', 'offset=', 'sort=nope', 'sort=a:up', 'sort=a:desc:x', 'sort=']) {
      const state = parseListState(new URLSearchParams(query), sortable);
      expect(state.offset, query).toBe(0);
      expect(state.sort, query).toEqual(DEFAULT_SORT);
    }
    expect(parseListState(new URLSearchParams('sort=a'), sortable).sort).toEqual({ field: 'a', desc: false });
    expect(parseListState(new URLSearchParams('filters='), sortable).filters).toBeNull();
  });
});

describe('sorting', () => {
  it('flips the same column and starts another ascending', () => {
    expect(nextSort({ field: 'a', desc: false }, 'a')).toEqual({ field: 'a', desc: true });
    expect(nextSort({ field: 'a', desc: true }, 'a')).toEqual({ field: 'a', desc: false });
    expect(nextSort({ field: 'a', desc: true }, 'b')).toEqual({ field: 'b', desc: false });
    expect(sortText({ field: 'a', desc: true })).toBe('a:desc');
    expect(sortText({ field: 'a', desc: false })).toBe('a');
  });
});

describe('formatCell', () => {
  it('formats by type and never produces markup', () => {
    expect(formatCell(null, 'data')).toBe('');
    expect(formatCell(undefined, 'int')).toBe('');
    expect(formatCell(true, 'check')).toBe('Yes');
    expect(formatCell(false, 'check')).toBe('No');
    expect(formatCell(12, 'int')).toBe('12');
    expect(formatCell('12.50', 'decimal')).toBe('12.50');
    expect(formatCell('2026-10-29T15:04:05.123456Z', 'datetime')).toBe('2026-10-29 15:04:05 UTC');
    expect(formatCell('2026-10-29T15:04:05Z', 'datetime')).toBe('2026-10-29 15:04:05 UTC');
    expect(formatCell('not a date', 'datetime')).toBe('not a date');
    expect(formatCell('<img src=x onerror=alert(1)>', 'data')).toBe('<img src=x onerror=alert(1)>');
  });
});
