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
type RowsState = { key: string; rows?: DocumentRow[]; error?: string };

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
        if (!cancelled) setRowsState({ key, rows });
      },
      (e: unknown) => {
        if (!cancelled) setRowsState({ key, error: message(e) });
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

  const current = rowsState?.key === key ? rowsState : null;
  const shown = current?.rows?.slice(0, PAGE_SIZE) ?? [];
  const hasNext = (current?.rows?.length ?? 0) > PAGE_SIZE;

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
        key={filtersText ?? ''}
        fields={fields}
        initial={parseFilters(filtersText)}
        onApply={(next) => go({ ...state, filters: next, offset: 0 })}
      />

      {current?.error ? (
        <p role="alert" className="error">
          {current.error}
        </p>
      ) : null}
      {current === null ? <p>Loading...</p> : null}

      {current?.rows ? (
        shown.length === 0 && offset === 0 ? (
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
                        aria-sort={sorted ? (state.sort.desc ? 'descending' : 'ascending') : 'none'}
                      >
                        <button
                          type="button"
                          className="sort"
                          onClick={() => go({ ...state, sort: nextSort(state.sort, column.key), offset: 0 })}
                        >
                          {column.label}
                          {sorted ? (state.sort.desc ? ' ▼' : ' ▲') : ''}
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
        <button type="button" disabled={offset === 0} onClick={() => go({ ...state, offset: Math.max(0, offset - PAGE_SIZE) })}>
          Previous
        </button>
        <span>{shown.length === 0 ? 'No rows' : `Rows ${offset + 1}–${offset + shown.length}`}</span>
        <button type="button" disabled={!hasNext} onClick={() => go({ ...state, offset: offset + PAGE_SIZE })}>
          Next
        </button>
      </nav>
    </main>
  );
}
