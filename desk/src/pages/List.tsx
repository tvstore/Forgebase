import { useEffect, useId, useMemo, useState } from 'react';

import { ApiError, getDocTypeMeta, listDocuments } from '../api';
import { Badge } from '../components/Badge';
import { EmptyState } from '../components/EmptyState';
import { FilterBar } from '../components/FilterBar';
import { Skeleton } from '../components/Skeleton';
import { parseFilters } from '../filters';
import {
  NAME_KEY,
  PAGE_SIZE,
  STATUS_KEY,
  filterFields,
  formatCell,
  isNumericColumn,
  listColumns,
  listStateParams,
  nextSort,
  parseListState,
  rowTitle,
  searchField,
  sortText,
  withSearch,
} from '../listing';
import { statusBadge } from '../status';
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
    listDocuments(doctype, {
      filters: requestFilters,
      sort,
      limit: PAGE_SIZE + 1,
      offset,
    }).then(
      (rows) => {
        if (!cancelled) setRowsState({ doctype, key, offset, rows });
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
  const filterCount = parseFilters(filtersText).length;
  const filtersId = useId();

  if (metaState?.doctype === doctype && metaState.error) {
    return (
      <div className="page">
        <h1>Cannot open this list</h1>
        <p role="alert" className="alert">
          {metaState.error}
        </p>
      </div>
    );
  }
  if (!meta) {
    return (
      <div className="page" aria-busy="true">
        <h1>Loading...</h1>
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

  return (
    <div className="page">
      <header className="page-head">
        <h1>{meta.label}</h1>
        {mayCreate ? (
          <a className="btn btn-primary" href={newHref(doctype)}>
            <span aria-hidden="true">+ </span>New
          </a>
        ) : null}
      </header>

      <div className="toolbar">
        <input
          type="search"
          className="search"
          aria-label={`Search ${meta.label} by ${search.label}`}
          placeholder={`Search by ${search.label.toLowerCase()}`}
          value={typed}
          onChange={(e) => setTyped(e.target.value)}
        />
        <button
          type="button"
          className="btn"
          aria-expanded={filtersOpen}
          aria-controls={filtersId}
          onClick={() => setFiltersOpen((o) => !o)}
        >
          Filters{filterCount > 0 ? ` (${filterCount})` : ''}
        </button>
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
        <div className="card" aria-busy="true">
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
                  <span aria-hidden="true">+ </span>New {meta.label}
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
                        column.key === STATUS_KEY ? 'col-status' : isNumericColumn(column) ? 'num' : undefined
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
                        {sorted ? <span aria-hidden="true">{state.sort.desc ? ' ▼' : ' ▲'}</span> : null}
                      </button>
                    </th>
                  );
                })}
              </tr>
            </thead>
            <tbody>
              {shown.map((row, index) => {
                const { title, name } = rowTitle(row, meta);
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
                          {meta.submittable ? (
                            <Badge tone={statusBadge(row[STATUS_KEY]).tone} className="status-inline">
                              {statusBadge(row[STATUS_KEY]).label}
                            </Badge>
                          ) : null}
                        </th>
                      ) : column.key === STATUS_KEY ? (
                        <td key={STATUS_KEY} className="col-status">
                          <Badge tone={statusBadge(row[STATUS_KEY]).tone}>{statusBadge(row[STATUS_KEY]).label}</Badge>
                        </td>
                      ) : (
                        <td key={column.key} className={isNumericColumn(column) ? 'num' : undefined}>
                          {formatCell(row[column.key], column.type)}
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
        <span className="paging-text">
          {shown.length === 0 ? 'No rows' : `Rows ${shownOffset + 1}–${shownOffset + shown.length}`}
        </span>
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
  );
}
