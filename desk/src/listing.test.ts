import { describe, expect, it } from 'vitest';

import {
  DEFAULT_SORT,
  filterFields,
  formatCell,
  formatDate,
  formatDateTime,
  groupDigits,
  isNumericColumn,
  listColumns,
  listStateParams,
  nextSort,
  parseListState,
  rowTitle,
  searchField,
  sortText,
  withSearch,
} from './listing';
import type { DocTypeMeta, FieldMeta, FieldType } from './types';

const field = (name: string, type: FieldType, extra: Partial<FieldMeta> = {}): FieldMeta => ({
  name,
  label: name.toUpperCase(),
  type,
  required: false,
  unique: false,
  allow_on_submit: false,
  read_only: false,
  ...extra,
});

const meta = (fields: FieldMeta[], submittable = false, extra: Partial<DocTypeMeta> = {}): DocTypeMeta => ({
  name: 'thing',
  label: 'Thing',
  app: null,
  naming: 'random',
  submittable,
  title_field: null,
  list_fields: null,
  fields,
  rights: {
    read: 'all', write: 'all', create: 'all', delete: 'all', submit: 'none', cancel: 'none', amend: 'none',
  },
  ...extra,
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

  it('uses list_fields in their order, skips unknown, Table and the title field, and adds the status', () => {
    const fields = [field('a', 'data'), field('b', 'int'), field('notes', 'text'), field('lines', 'table')];
    const columns = listColumns(
      meta(fields, true, { title_field: 'a', list_fields: ['notes', 'a', 'gone', 'lines', 'b'] }),
    );
    expect(columns.map((c) => c.key)).toEqual(['name', 'notes', 'b', 'docstatus', 'modified_at']);
    expect(columns.find((c) => c.key === 'docstatus')?.label).toBe('Status');
  });

  it('names the primary column after the title field, with "Name" muted under it', () => {
    const columns = listColumns(meta([field('a', 'data')], false, { title_field: 'a' }));
    expect(columns[0]).toMatchObject({ key: 'name', label: 'A', sub: 'Name' });
    const plain = listColumns(meta([field('a', 'data')]));
    expect(plain[0]).toEqual({ key: 'name', label: 'Name', type: 'data' });
  });

  it('falls back to the default columns without the title field, and has no status when not submittable', () => {
    const columns = listColumns(meta([field('a', 'data'), field('b', 'int')], false, { title_field: 'a' }));
    expect(columns.map((c) => c.key)).toEqual(['name', 'b', 'modified_at']);
  });

  it('right-aligns numbers only', () => {
    const columns = listColumns(meta([field('a', 'data'), field('b', 'int'), field('c', 'decimal')]));
    expect(columns.filter(isNumericColumn).map((c) => c.key)).toEqual(['b', 'c']);
  });
});

describe('rowTitle', () => {
  const m = meta([field('a', 'data')], false, { title_field: 'a' });
  it('is the title field value, else the name', () => {
    expect(rowTitle({ name: 'N1', a: 'Ada' }, m)).toEqual({ title: 'Ada', name: 'N1' });
    expect(rowTitle({ name: 'N1', a: '' }, m)).toEqual({ title: 'N1', name: 'N1' });
    expect(rowTitle({ name: 'N1', a: null }, m)).toEqual({ title: 'N1', name: 'N1' });
    expect(rowTitle({ name: 'N1', a: 'Ada' }, meta([]))).toEqual({ title: 'N1', name: 'N1' });
  });
});

describe('quick search', () => {
  it('looks in the title field when like is allowed on it, else in name', () => {
    const fields = [field('a', 'data'), field('c', 'link'), field('s', 'select')];
    expect(searchField(meta(fields, false, { title_field: 'a' }))).toEqual({ name: 'a', label: 'A' });
    expect(searchField(meta(fields, false, { title_field: 's' })).name).toBe('s');
    expect(searchField(meta(fields, false, { title_field: 'c' })).name).toBe('name'); // like is refused on a Link
    expect(searchField(meta(fields, false, { title_field: 'gone' })).name).toBe('name');
    expect(searchField(meta(fields))).toEqual({ name: 'name', label: 'Name' });
  });

  it('builds a like filter on top of the user filters', () => {
    expect(withSearch(null, '', 'name')).toBeUndefined();
    expect(withSearch('[{"field":"a","op":"is null"}]', '  ', 'name')).toBe('[{"field":"a","op":"is null"}]');
    expect(JSON.parse(withSearch(null, ' ada ', 'name') ?? '')).toEqual([
      { field: 'name', op: 'like', value: '%ada%' },
    ]);
    expect(JSON.parse(withSearch('[{"field":"a","op":"is null"}]', 'x', 'a') ?? '')).toEqual([
      { field: 'a', op: 'is null' },
      { field: 'a', op: 'like', value: '%x%' },
    ]);
  });

  it('matches the typed text literally and leaves a bad filters text to the server', () => {
    expect(JSON.parse(withSearch(null, '50%_\\', 'name') ?? '')).toEqual([
      { field: 'name', op: 'like', value: '%50\\%\\_\\\\%' },
    ]);
    expect(withSearch('not json', 'x', 'name')).toBe('not json');
    expect(withSearch('{"a":1}', 'x', 'name')).toBe('{"a":1}');
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
      q: '',
    });
    expect(listStateParams({ offset: 0, sort: DEFAULT_SORT, filters: null, q: '' }).toString()).toBe('');
  });

  it('round-trips offset, sort, filters and the search text', () => {
    const state = { offset: 40, sort: { field: 'a', desc: true }, filters: '[{"field":"a","op":"is null"}]', q: 'ada & co' };
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
    expect(formatCell(1234567, 'int')).toBe('1,234,567');
    expect(formatCell('12.50', 'decimal')).toBe('12.50');
    expect(formatCell('1234.50', 'decimal')).toBe('1,234.50');
    expect(formatCell('2026-10-29T15:04:05.123456Z', 'datetime')).toBe('29 Oct 2026, 15:04 UTC');
    expect(formatCell('2026-10-29T15:04:05Z', 'datetime')).toBe('29 Oct 2026, 15:04 UTC');
    expect(formatCell('2026-10-29', 'date')).toBe('29 Oct 2026');
    expect(formatCell('not a date', 'datetime')).toBe('not a date');
    expect(formatCell('<img src=x onerror=alert(1)>', 'data')).toBe('<img src=x onerror=alert(1)>');
  });
});

describe('number and date helpers', () => {
  it('groups digits without changing them', () => {
    expect(groupDigits('0')).toBe('0');
    expect(groupDigits('999')).toBe('999');
    expect(groupDigits('1000')).toBe('1,000');
    expect(groupDigits('-1234567.5000')).toBe('-1,234,567.5000');
    expect(groupDigits('1e5')).toBe('1e5');
    expect(groupDigits('abc')).toBe('abc');
  });

  it('formats dates and leaves other text alone', () => {
    expect(formatDate('2026-01-05')).toBe('5 Jan 2026');
    expect(formatDate('2026-13-05')).toBe('2026-13-05');
    expect(formatDate('yesterday')).toBe('yesterday');
    expect(formatDateTime('2026-12-31T23:59:59Z')).toBe('31 Dec 2026, 23:59 UTC');
    expect(formatDateTime('2026-12-31 23:59')).toBe('2026-12-31 23:59');
  });
});
