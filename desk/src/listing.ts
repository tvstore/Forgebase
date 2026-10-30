// Pure helpers of the list view: which columns to show, the paging/sort state kept in the
// address, and how a cell is displayed.

import type { FilterField } from './filters';
import { escapeLike } from './links';
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
  /** A muted second line of the header: what the column sorts by when `label` is the title field. */
  sub?: string;
}

export interface Sort {
  field: string;
  desc: boolean;
}

const isNumeric = (type: FieldType): boolean => type === 'int' || type === 'decimal';

/** Numbers are right-aligned in the list. */
export const isNumericColumn = (column: Column): boolean => isNumeric(column.type);

/** The column that holds the link to the document (its title, with the name muted beside it). */
export const NAME_KEY = 'name';
/** The status badge column of a submittable DocType. */
export const STATUS_KEY = 'docstatus';

/**
 * `name` first (the primary cell), then `list_fields` of the metadata (or, when it names none,
 * the first five fields that are not Table or Text), then the status of a submittable DocType and
 * `modified_at`. The title field is not repeated: the primary cell already shows it, and its
 * header carries the title field's label (with "Name" muted under it, as in the cells).
 */
export function listColumns(meta: DocTypeMeta): Column[] {
  const known = new Map(meta.fields.map((f) => [f.name, f]));
  // The primary cell shows the title field: its header says so (the column still sorts by name).
  const titleField = meta.title_field === null ? undefined : known.get(meta.title_field);
  const wanted =
    meta.list_fields === null
      ? meta.fields.filter((f) => f.type !== 'table' && f.type !== 'text').map((f) => f.name)
      : meta.list_fields;
  const fields: Column[] = [];
  for (const name of wanted) {
    const f = known.get(name);
    if (!f || f.type === 'table' || f.name === NAME_KEY || f.name === meta.title_field) continue;
    fields.push({ key: f.name, label: f.label, type: f.type });
  }
  return [
    titleField === undefined
      ? { key: NAME_KEY, label: 'Name', type: 'data' as const }
      : { key: NAME_KEY, label: titleField.label, type: 'data' as const, sub: 'Name' },
    ...(meta.list_fields === null ? fields.slice(0, MAX_FIELD_COLUMNS) : fields),
    ...(meta.submittable ? [{ key: STATUS_KEY, label: 'Status', type: 'data' as const }] : []),
    { key: 'modified_at', label: 'Modified', type: 'datetime' },
  ];
}

/** The primary text of a listed document: the value of the title field, else the name. */
export function rowTitle(row: Record<string, unknown>, meta: DocTypeMeta): { title: string; name: string } {
  const name = String(row[NAME_KEY] ?? '');
  const value = meta.title_field === null ? null : row[meta.title_field];
  const title = typeof value === 'string' && value !== '' ? value : name;
  return { title, name };
}

/**
 * The field the quick search looks in: the title field when `like` is allowed on it (Data, Text
 * and Select fields), else `name`.
 */
export function searchField(meta: DocTypeMeta): { name: string; label: string } {
  const f = meta.title_field === null ? undefined : meta.fields.find((x) => x.name === meta.title_field);
  return f && (f.type === 'data' || f.type === 'text' || f.type === 'select')
    ? { name: f.name, label: f.label }
    : { name: NAME_KEY, label: 'Name' };
}

/**
 * The `filters` of the API for the user's filters (the JSON text of the address) plus the quick
 * search: `field like %text%` (the typed text matches literally). Undefined when there is neither.
 * A `filters` text that is not a JSON array is passed on unchanged: the server refuses it.
 */
export function withSearch(filters: string | null, search: string, field: string): string | undefined {
  const text = search.trim();
  if (text === '') return filters ?? undefined;
  let list: unknown[] = [];
  if (filters) {
    try {
      const parsed: unknown = JSON.parse(filters);
      if (!Array.isArray(parsed)) return filters;
      list = parsed;
    } catch {
      return filters;
    }
  }
  return JSON.stringify([...list, { field, op: 'like', value: `%${escapeLike(text)}%` }]);
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
  /** The quick search text ("" for none). */
  q: string;
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
  return { offset, sort, filters: filters === '' ? null : filters, q: params.get('q') ?? '' };
}

/** The query part of the address for a list state (defaults are left out). */
export function listStateParams(state: ListState): URLSearchParams {
  const params = new URLSearchParams();
  if (state.q !== '') params.set('q', state.q);
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

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

/** `1234567` and `-1234567.50` as `1,234,567` and `-1,234,567.50` (the digits are kept as they are). */
export function groupDigits(text: string): string {
  const match = /^(-?)(\d+)(\.\d+)?$/.exec(text);
  if (!match) return text;
  const [, sign = '', whole = '', fraction = ''] = match;
  return `${sign}${whole.replace(/\B(?=(\d{3})+$)/g, ',')}${fraction}`;
}

/** `2026-10-29` as `29 Oct 2026`; anything else as it is. */
export function formatDate(value: string): string {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/.exec(value);
  const month = match ? MONTHS[Number(match[2]) - 1] : undefined;
  return match && month ? `${Number(match[3])} ${month} ${match[1]}` : value;
}

/** `2026-10-29T15:04:05.123Z` as `29 Oct 2026, 15:04 UTC`; anything else as it is. */
export function formatDateTime(value: string): string {
  const match = /^(\d{4}-\d{2}-\d{2})T(\d{2}:\d{2}):\d{2}(?:\.\d+)?Z$/.exec(value);
  return match ? `${formatDate(match[1] ?? '')}, ${match[2]} UTC` : value;
}

/** A cell as text. React escapes it; nothing here is ever HTML. */
export function formatCell(value: unknown, type: FieldType): string {
  if (value === null || value === undefined) return '';
  if (type === 'check') return value === true ? 'Yes' : 'No';
  if (typeof value === 'string') {
    if (type === 'datetime') return formatDateTime(value);
    if (type === 'date') return formatDate(value);
    if (type === 'decimal') return groupDigits(value);
    return value;
  }
  if (type === 'int' && typeof value === 'number') return groupDigits(String(value));
  return String(value);
}
