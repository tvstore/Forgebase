import { useEffect, useId, useMemo, useState } from 'react';

import { ApiError, getDocTypeMeta, listDocumentsWithTitles } from '../api';
import { Badge } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { FilterBar } from '../components/FilterBar';
import { PageHeader } from '../components/PageHeader';
import { Skeleton } from '../components/Skeleton';
import { filterChips, parseFilters, withoutFilter } from '../filters';
import { FunnelIcon, PlusIcon, SearchIcon, XIcon } from '../icons';
import { NO_TITLES } from '../linkTitles';
import type { LinkTitles } from '../linkTitles';
import {
  MODIFIED_KEY,
  NAME_KEY,
  OWNER_KEY,
  PAGE_SIZE,
  filterFields,
  formatCell,
  hasStatus,
  isNumericColumn,
  isStatusKey,
  listColumns,
  listStateParams,
  nextSort,
  parseListState,
  rowTitle,
  searchField,
  showingText,
  sortText,
  withSearch,
} from '../listing';
import { initials } from '../people';
import { stateBadge } from '../status';
import { exactTime, relativeTime } from '../time';
import type { ListState } from '../listing';
import { formHref, listHref, navigate, newHref } from '../router';
import type { DocTypeMeta, DocumentRow } from '../types';

interface Props {
  doctype: string;
  params: URLSearchParams;
}

type MetaState = { doctype: string; meta?: DocTypeMeta; error?: string };
/**
 * The last answer. `rows` stay on screen (and keep the keyboard focus on the controls) while the
 * next request loads; `offset` is the offset they belong to; `error` is that of request `key`.
 */
type RowsState = {
  doctype: string;
  key: string;
  offset: number;
  rows?: DocumentRow[];
  /** The titles of the documents the rows link to (only those the user may read). */
  titles?: LinkTitles;
  /** When the answer arrived: the clock of the relative "Modified" times. */
  at?: Date;
  error?: string;
};

const message = (e: unknown): string =>
  e instanceof ApiError ? e.message : 'Cannot reach the server.';

/** How long after the last key the quick search is applied. */
const SEARCH_DELAY_MS = 300;

export function List({ doctype, params }: Props) {
  const [metaState, setMetaState] = useState<MetaState | null>(null);
  const [rowsState, setRowsState] = useState<RowsState | null>(null);

  useEffect(() => {
    let cancelled = false;
    getDocTypeMeta(doctype).then(
      (meta) => {
        if (!cancelled) setMetaState({ doctype, meta });
      },
      (e: unknown) => {
        if (!cancelled) setMetaState({ doctype, error: message(e) });
      },
    );
    return () => {
      cancelled = true;
    };
  }, [doctype]);

  const meta = metaState?.doctype === doctype ? metaState.meta : undefined;
  const columns = useMemo(() => (meta ? listColumns(meta) : []), [meta]);
  const fields = useMemo(() => (meta ? filterFields(meta) : []), [meta]);
  const sortable = useMemo(() => columns.map((c) => c.key), [columns]);
  const state: ListState = useMemo(() => parseListState(params, sortable), [params, sortable]);
  const filtersText = state.filters;
  const sort = sortText(state.sort);
  const offset = state.offset;
  const search = meta ? searchField(meta) : { name: NAME_KEY, label: 'Name' };
  const requestFilters = withSearch(filtersText, state.q, search.name);
  // One key per request: while it differs from the stored result, the list is loading.
  const key = `${doctype}|${offset}|${sort}|${requestFilters ?? ''}`;

  useEffect(() => {
    if (!meta) return;
    let cancelled = false;
    // One more row than a page, to know whether there is a next page.
    listDocumentsWithTitles(doctype, {
      filters: requestFilters,
      sort,
      limit: PAGE_SIZE + 1,
      offset,
    }).then(
      ({ rows, titles }) => {
        if (!cancelled) setRowsState({ doctype, key, offset, rows, titles, at: new Date() });
      },
      (e: unknown) => {
        if (cancelled) return;
        // No rows: the old ones belong to another request and must not pass for this result.
        // The table (its sort buttons) and the paging controls stay mounted, so keyboard focus
        // stays where it is.
        setRowsState({ doctype, key, offset, error: message(e) });
      },
    );
    return () => {
      cancelled = true;
    };
  }, [meta, doctype, requestFilters, sort, offset, key]);

  const go = (next: ListState) => navigate(listHref(doctype, listStateParams(next)));

  // The search box: the text is local while typing and goes to the address a moment after the
  // last key (so it survives a reload and the back button); a change of the address (back button,
  // "Clear") is followed without losing the focus.
  const [typed, setTyped] = useState(state.q);
  const [seenQ, setSeenQ] = useState(state.q);
  if (seenQ !== state.q) {
    setSeenQ(state.q);
    setTyped(state.q);
  }
  useEffect(() => {
    if (typed === state.q) return;
    const timer = window.setTimeout(
      () => navigate(listHref(doctype, listStateParams({ ...state, q: typed, offset: 0 }))),
      SEARCH_DELAY_MS,
    );
    return () => window.clearTimeout(timer);
  }, [typed, state, doctype]);

  const [filtersOpen, setFiltersOpen] = useState(filtersText !== null);
  const chips = useMemo(() => filterChips(filtersText, fields), [filtersText, fields]);
  const filtersId = useId();

  if (metaState?.doctype === doctype && metaState.error) {
    return (
      <div className="page">
        <PageHeader title={<h1>Cannot open this list</h1>} />
        <p role="alert" className="alert">
          {metaState.error}
        </p>
      </div>
    );
  }
  if (!meta) {
    return (
      <div className="page" aria-busy="true">
        <PageHeader title={<h1>Loading...</h1>} />
        <Skeleton lines={5} />
      </div>
    );
  }

  // The last answer for this DocType stays on screen while a new request loads.
  const answer = rowsState?.doctype === doctype ? rowsState : null;
  const loading = answer?.key !== key;
  const shown = answer?.rows?.slice(0, PAGE_SIZE) ?? [];
  const shownOffset = answer?.offset ?? offset;
  const hasNext = (answer?.rows?.length ?? 0) > PAGE_SIZE;
  const filtered = state.q.trim() !== '' || filtersText !== null;
  const mayCreate = meta.rights.create !== 'none';
  const empty = !!answer?.rows && shown.length === 0 && shownOffset === 0 && !answer.error;

  const titles = answer?.titles ?? NO_TITLES;
  const now = answer?.at ?? new Date(0);

  return (
    <div className="page">
      <PageHeader
        title={<h1>{meta.label}</h1>}
        actions={
          mayCreate ? (
            <a className="btn btn-primary" href={newHref(doctype)}>
              <PlusIcon size={16} />
              New
            </a>
          ) : null
        }
      />

      <div className="list-card">
        <div className="toolbar">
          <div className="search-wrap">
            <SearchIcon size={16} className="icon" />
            <input
              type="search"
              className="search"
              aria-label={`Search ${meta.label} by ${search.label}`}
              placeholder={`Search by ${search.label.toLowerCase()}`}
              value={typed}
              onChange={(e) => setTyped(e.target.value)}
            />
          </div>
          {chips.length > 0 ? (
            <ul className="chips" aria-label="Active filters">
              {chips.map((chip) => (
                <li key={`${chip.index}-${chip.label}`} className="chip">
                  <span>{chip.label}</span>
                  <button
                    type="button"
                    aria-label={`Remove filter: ${chip.label}`}
                    onClick={() => go({ ...state, filters: withoutFilter(filtersText, chip.index, fields), offset: 0 })}
                  >
                    <XIcon size={14} />
                  </button>
                </li>
              ))}
            </ul>
          ) : null}
          <button
            type="button"
            className="pill"
            aria-expanded={filtersOpen}
            aria-controls={filtersId}
            onClick={() => setFiltersOpen((o) => !o)}
          >
            {chips.length > 0 ? <FunnelIcon size={14} /> : <PlusIcon size={14} />}
            Filter
          </button>
          <span className="list-count" role="status" aria-live="polite">
            {answer?.rows ? showingText(shownOffset, shown.length) : ''}
          </span>
        </div>
        {filtersOpen ? (
          <div id={filtersId}>
            <FilterBar
              initialKey={filtersText ?? ''}
              fields={fields}
              initial={parseFilters(filtersText)}
              onApply={(next) => go({ ...state, filters: next, offset: 0 })}
            />
          </div>
        ) : null}

        {!loading && answer?.error ? (
          <p role="alert" className="alert">
            {answer.error}
          </p>
        ) : null}

        {!answer ? (
          <div className="skeleton-pad" aria-busy="true">
            <Skeleton lines={6} />
          </div>
        ) : empty ? (
          filtered ? (
            <EmptyState
              title="No documents match"
              action={
                <button type="button" className="btn" onClick={() => go({ ...state, q: '', filters: null, offset: 0 })}>
                  Clear search and filters
                </button>
              }
            >
              Try a different search, or remove a filter.
            </EmptyState>
          ) : (
            <EmptyState
              title={`No ${meta.label} yet`}
              action={
                mayCreate ? (
                  <a className="btn btn-primary" href={newHref(doctype)}>
                    <PlusIcon size={16} />
                    New {meta.label}
                  </a>
                ) : undefined
              }
            >
              {mayCreate ? 'Create the first one to get started.' : 'There is nothing here to show you.'}
            </EmptyState>
          )
        ) : (
          <div className={loading ? 'table-wrap is-loading' : 'table-wrap'} aria-busy={loading}>
            <table className="list-table">
              <caption className="visually-hidden">{meta.label} documents</caption>
              <thead>
                <tr>
                  {columns.map((column) => {
                    const sorted = state.sort.field === column.key;
                    return (
                      <th
                        key={column.key}
                        scope="col"
                        className={
                          isStatusKey(column.key) ? 'col-status' : isNumericColumn(column) ? 'num' : undefined
                        }
                        aria-sort={sorted ? (state.sort.desc ? 'descending' : 'ascending') : undefined}
                      >
                        <button
                          type="button"
                          className="sort"
                          onClick={() => go({ ...state, sort: nextSort(state.sort, column.key), offset: 0 })}
                        >
                          {column.label}
                          {column.sub ? <span className="sort-sub">{column.sub}</span> : null}
                          {sorted ? <span aria-hidden="true">{state.sort.desc ? ' \u25BC' : ' \u25B2'}</span> : null}
                        </button>
                      </th>
                    );
                  })}
                </tr>
              </thead>
              <tbody>
                {shown.map((row, index) => {
                  const { title, name } = rowTitle(row, meta, titles);
                  const href = formHref(doctype, name);
                  return (
                    <tr
                      key={name || index}
                      className="row-link"
                      onClick={(event) => {
                        // A convenience for the mouse: the link in the first cell is the real control.
                        if (!(event.target instanceof Element && event.target.closest('a, button'))) navigate(href);
                      }}
                    >
                      {columns.map((column) =>
                        column.key === NAME_KEY ? (
                          <th key={NAME_KEY} scope="row">
                            <a href={href}>{title}</a>
                            {title !== name ? <span className="muted-name">{name}</span> : null}
                            {hasStatus(meta) ? (
                              <Badge tone={stateBadge(meta, row).tone} className="status-inline">
                                {stateBadge(meta, row).label}
                              </Badge>
                            ) : null}
                          </th>
                        ) : isStatusKey(column.key) ? (
                          <td key={column.key} className="col-status">
                            <Badge tone={stateBadge(meta, row).tone}>{stateBadge(meta, row).label}</Badge>
                          </td>
                        ) : column.key === OWNER_KEY ? (
                          <td key={OWNER_KEY}>
                            {typeof row[OWNER_KEY] === 'string' && row[OWNER_KEY] !== '' ? (
                              <span className="person">
                                <span className="avatar-sm" aria-hidden="true">
                                  {initials(row[OWNER_KEY])}
                                </span>
                                <span>{row[OWNER_KEY]}</span>
                              </span>
                            ) : null}
                          </td>
                        ) : column.key === MODIFIED_KEY ? (
                          <td key={MODIFIED_KEY} className="muted">
                            {typeof row[MODIFIED_KEY] === 'string' ? (
                              <time dateTime={row[MODIFIED_KEY]} title={exactTime(row[MODIFIED_KEY])}>
                                {relativeTime(row[MODIFIED_KEY], now)}
                              </time>
                            ) : null}
                          </td>
                        ) : (
                          <td key={column.key} className={isNumericColumn(column) ? 'num' : undefined}>
                            {formatCell(row[column.key], column.type, column.target, titles)}
                          </td>
                        ),
                      )}
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
        <p className="visually-hidden" role="status" aria-live="polite">
          {loading ? 'Loading...' : ''}
        </p>

        <nav className="paging" aria-label="Pages">
          {/* aria-disabled, not disabled: the button that was just used may become unavailable
              (the first or last page) and must keep the keyboard focus. */}
          {/* Not while a request is running: `hasNext` and the rows still belong to the previous
              request, so a click then could go past the last page. */}
          <button
            type="button"
            className="btn btn-small"
            aria-disabled={offset === 0 || loading}
            onClick={() => {
              if (offset > 0 && !loading) go({ ...state, offset: Math.max(0, offset - PAGE_SIZE) });
            }}
          >
            Previous
          </button>
          <button
            type="button"
            className="btn btn-small"
            aria-disabled={!hasNext || loading}
            onClick={() => {
              if (hasNext && !loading) go({ ...state, offset: offset + PAGE_SIZE });
            }}
          >
            Next
          </button>
        </nav>
      </div>
    </div>
  );
}
