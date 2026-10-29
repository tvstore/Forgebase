// Pure helpers of the list view: which columns to show, the paging/sort state kept in the
// address, and how a cell is displayed.

import type { FilterField } from './filters';
import type { DocTypeMeta, FieldType } from './types';

/** The page size of the list view. */
export const PAGE_SIZE = 20;
/** The most field columns shown besides `name` and `modified_at`. */
export const MAX_FIELD_COLUMNS = 5;
/** The sort when the address names none: the most recently changed first. */
export const DEFAULT_SORT: Sort = { field: 'modified_at', desc: true };

export interface Column {
  /** The key in the listed document and the name used in `sort`. */
  key: string;
  label: string;
  type: FieldType;
}

export interface Sort {
  field: string;
  desc: boolean;
}

/** `name`, the first few fields (not Table, not Text) and `modified_at`. */
export function listColumns(meta: DocTypeMeta): Column[] {
  const fields: Column[] = meta.fields
    .filter((f) => f.type !== 'table' && f.type !== 'text' && f.name !== 'name')
    .slice(0, MAX_FIELD_COLUMNS)
    .map((f) => ({ key: f.name, label: f.label, type: f.type }));
  return [
    { key: 'name', label: 'Name', type: 'data' },
    ...fields,
    { key: 'modified_at', label: 'Modified', type: 'datetime' },
  ];
}

/** Every field the filter bar offers: the standard columns and every field except Table fields. */
export function filterFields(meta: DocTypeMeta): FilterField[] {
  const standard: FilterField[] = [
    { name: 'name', label: 'Name', type: 'data' },
    { name: 'owner', label: 'Owner', type: 'data' },
    { name: 'modified_at', label: 'Modified', type: 'datetime' },
    ...(meta.submittable
      ? [{ name: 'docstatus', label: 'Document status (0 draft, 1 submitted, 2 cancelled)', type: 'int' as const }]
      : []),
  ];
  const own: FilterField[] = meta.fields
    .filter((f) => f.type !== 'table')
    .map((f) => ({
      name: f.name,
      label: f.label,
      type: f.type,
      ...(f.type === 'select' && f.options ? { options: f.options } : {}),
    }));
  return [...standard, ...own];
}

export interface ListState {
  offset: number;
  sort: Sort;
  /** The `filters` JSON text as it is in the address, or null. */
  filters: string | null;
}

/** Reads the paging, sort and filters from the query of the address; bad values are ignored. */
export function parseListState(params: URLSearchParams, sortable: string[]): ListState {
  const offsetText = params.get('offset') ?? '';
  const offset = /^\d{1,9}$/.test(offsetText) ? Number(offsetText) : 0;
  let sort = DEFAULT_SORT;
  const sortText = params.get('sort');
  if (sortText !== null) {
    const [field = '', direction, ...rest] = sortText.split(':');
    if (rest.length === 0 && sortable.includes(field) && (direction === undefined || direction === 'asc' || direction === 'desc')) {
      sort = { field, desc: direction === 'desc' };
    }
  }
  const filters = params.get('filters');
  return { offset, sort, filters: filters === '' ? null : filters };
}

/** The query part of the address for a list state (defaults are left out). */
export function listStateParams(state: ListState): URLSearchParams {
  const params = new URLSearchParams();
  if (state.filters) params.set('filters', state.filters);
  if (state.sort.field !== DEFAULT_SORT.field || state.sort.desc !== DEFAULT_SORT.desc) {
    params.set('sort', sortText(state.sort));
  }
  if (state.offset > 0) params.set('offset', String(state.offset));
  return params;
}

/** The `sort` parameter of the API: `field` or `field:desc`. */
export function sortText(sort: Sort): string {
  return sort.desc ? `${sort.field}:desc` : sort.field;
}

/** The sort after a click on a column header: the same column flips, another starts ascending. */
export function nextSort(current: Sort, field: string): Sort {
  return current.field === field ? { field, desc: !current.desc } : { field, desc: false };
}

/** A cell as text. React escapes it; nothing here is ever HTML. */
export function formatCell(value: unknown, type: FieldType): string {
  if (value === null || value === undefined) return '';
  if (type === 'check') return value === true ? 'Yes' : 'No';
  if (type === 'datetime' && typeof value === 'string') {
    // 2026-10-29T15:04:05.123456Z -> 2026-10-29 15:04:05 UTC
    const match = /^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2}:\d{2})(?:\.\d+)?Z$/.exec(value);
    if (match) return `${match[1]} ${match[2]} UTC`;
  }
  return typeof value === 'string' ? value : String(value);
}
