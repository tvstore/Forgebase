import { useEffect, useMemo, useState } from 'react';

import { ApiError, getDocTypeMeta, listDocuments } from '../api';
import { FilterBar } from '../components/FilterBar';
import { parseFilters } from '../filters';
import {
  PAGE_SIZE,
  filterFields,
  formatCell,
  listColumns,
  listStateParams,
  nextSort,
  parseListState,
  sortText,
} from '../listing';
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
  const state: ListState = parseListState(
    params,
    columns.map((c) => c.key),
  );
  const filtersText = state.filters;
  const sort = sortText(state.sort);
  const offset = state.offset;
  // One key per request: while it differs from the stored result, the list is loading.
  const key = `${doctype}|${offset}|${sort}|${filtersText ?? ''}`;

  useEffect(() => {
    if (!meta) return;
    let cancelled = false;
    // One more row than a page, to know whether there is a next page.
    listDocuments(doctype, {
      filters: filtersText ?? undefined,
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
  }, [meta, doctype, filtersText, sort, offset, key]);

  const go = (next: ListState) => navigate(listHref(doctype, listStateParams(next)));

  if (metaState?.doctype === doctype && metaState.error) {
    return (
      <main>
        <h1>{doctype}</h1>
        <p role="alert" className="error">
          {metaState.error}
        </p>
      </main>
    );
  }
  if (!meta) {
    return (
      <main>
        <h1>{doctype}</h1>
        <p>Loading...</p>
      </main>
    );
  }

  // The last answer for this DocType stays on screen while a new request loads.
  const answer = rowsState?.doctype === doctype ? rowsState : null;
  const loading = answer?.key !== key;
  const shown = answer?.rows?.slice(0, PAGE_SIZE) ?? [];
  const shownOffset = answer?.offset ?? offset;
  const hasNext = (answer?.rows?.length ?? 0) > PAGE_SIZE;

  return (
    <main>
      <div className="title-bar">
        <h1>{meta.label}</h1>
        {meta.rights.create !== 'none' ? (
          <a className="button primary" href={newHref(doctype)}>
            New
          </a>
        ) : null}
      </div>

      <FilterBar
        initialKey={filtersText ?? ''}
        fields={fields}
        initial={parseFilters(filtersText)}
        onApply={(next) => go({ ...state, filters: next, offset: 0 })}
      />

      {!loading && answer?.error ? (
        <p role="alert" className="error">
          {answer.error}
        </p>
      ) : null}
      {/* Always in the page, so that a screen reader announces the change. */}
      <p role="status" aria-live="polite" className="loading">
        {loading ? 'Loading...' : ''}
      </p>

      {answer?.rows || answer?.error ? (
        shown.length === 0 && shownOffset === 0 && !answer.error ? (
          <p>No documents.</p>
        ) : (
          <div className="table-wrap">
            <table>
              <caption className="visually-hidden">{meta.label} documents</caption>
              <thead>
                <tr>
                  {columns.map((column) => {
                    const sorted = state.sort.field === column.key;
                    return (
                      <th
                        key={column.key}
                        scope="col"
                        aria-sort={sorted ? (state.sort.desc ? 'descending' : 'ascending') : undefined}
                      >
                        <button
                          type="button"
                          className="sort"
                          onClick={() => go({ ...state, sort: nextSort(state.sort, column.key), offset: 0 })}
                        >
                          {column.label}
                          {sorted ? (
                            <span aria-hidden="true">{state.sort.desc ? ' ▼' : ' ▲'}</span>
                          ) : null}
                        </button>
                      </th>
                    );
                  })}
                </tr>
              </thead>
              <tbody>
                {shown.map((row, index) => {
                  const name = String(row.name ?? '');
                  return (
                    <tr key={name || index}>
                      {columns.map((column) =>
                        column.key === 'name' ? (
                          <th key="name" scope="row">
                            <a href={formHref(doctype, name)}>{name}</a>
                          </th>
                        ) : (
                          <td key={column.key}>{formatCell(row[column.key], column.type)}</td>
                        ),
                      )}
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )
      ) : null}

      <nav className="paging" aria-label="Pages">
        {/* aria-disabled, not disabled: the button that was just used may become unavailable
            (the first or last page) and must keep the keyboard focus. */}
        {/* Not while a request is running: `hasNext` and the rows still belong to the previous
            request, so a click then could go past the last page. */}
        <button
          type="button"
          aria-disabled={offset === 0 || loading}
          onClick={() => {
            if (offset > 0 && !loading) go({ ...state, offset: Math.max(0, offset - PAGE_SIZE) });
          }}
        >
          Previous
        </button>
        <span>
          {shown.length === 0 ? 'No rows' : `Rows ${shownOffset + 1}–${shownOffset + shown.length}`}
        </span>
        <button
          type="button"
          aria-disabled={!hasNext || loading}
          onClick={() => {
            if (hasNext && !loading) go({ ...state, offset: offset + PAGE_SIZE });
          }}
        >
          Next
        </button>
      </nav>
    </main>
  );
}
