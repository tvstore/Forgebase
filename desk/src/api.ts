// The fetch wrapper of the desk: JSON in and out, the error envelope as a typed error and the
// CSRF header. The CSRF token is kept in this module's memory only: it is never written to any
// browser storage (the eslint config forbids it). After a page reload it is recovered with
// `GET /api/v1/session`, which issues a new one; a request refused with `csrf_failed` (the token
// was rotated by another tab) refreshes it once and is retried once.

import { isAddressable } from './access';
import { parseLinkTitles } from './linkTitles';
import type { LinkTitles } from './linkTitles';
import type { DocTypeMeta, DocTypeSummary, DocumentRow, SessionInfo } from './types';

const CSRF_HEADER = 'X-Forgebase-CSRF';

/** An error answered by the API (`{"error": {"code", "message", "details"?}}`). */
export class ApiError extends Error {
  readonly status: number;
  readonly code: string;
  readonly details: string[];

  constructor(status: number, code: string, message: string, details: string[] = []) {
    super(message);
    this.name = 'ApiError';
    this.status = status;
    this.code = code;
    this.details = details;
  }
}

let csrfToken: string | null = null;
let onUnauthenticated: (() => void) | null = null;
let refreshing: Promise<void> | null = null;

export function setCsrfToken(token: string | null): void {
  csrfToken = token;
}

/** Called when a request that needed a session gets 401 (the session ended). */
export function setUnauthenticatedHandler(handler: (() => void) | null): void {
  onUnauthenticated = handler;
}

interface Options {
  body?: unknown;
  /** The request is allowed to be 401 without meaning "the session ended" (login, session probe). */
  expectUnauthenticated?: boolean;
  /** `GET /api/v1/session` only: the header that marks a deliberate refresh of the CSRF token. */
  refresh?: boolean;
  /** Internal: this request is the retry after a CSRF refresh. */
  retried?: boolean;
  /** Returns the whole answer (`data` and the keys beside it) instead of `data` only. */
  envelope?: boolean;
}

async function parse(response: Response): Promise<unknown> {
  if (response.status === 204) return undefined;
  try {
    return await response.json();
  } catch {
    return undefined;
  }
}

function toError(status: number, payload: unknown): ApiError {
  const error = (payload as { error?: Record<string, unknown> } | undefined)?.error;
  if (error && typeof error.code === 'string' && typeof error.message === 'string') {
    const details = Array.isArray(error.details)
      ? error.details.filter((d): d is string => typeof d === 'string')
      : [];
    return new ApiError(status, error.code, error.message, details);
  }
  return new ApiError(status, 'unexpected', `unexpected response (HTTP ${status})`);
}

async function refreshCsrf(): Promise<void> {
  // One refresh at a time: two parallel refreshes would invalidate each other's token.
  refreshing ??= getSession()
    .then(() => undefined)
    .catch((e: unknown) => {
      // No session any more: the same as any other 401, so the user lands on the login page.
      if (e instanceof ApiError && e.status === 401) {
        csrfToken = null;
        onUnauthenticated?.();
      }
      throw e;
    })
    .finally(() => {
      refreshing = null;
    });
  await refreshing;
}

async function request<T>(method: string, path: string, options: Options = {}): Promise<T> {
  const headers: Record<string, string> = { Accept: 'application/json' };
  if (options.body !== undefined) headers['Content-Type'] = 'application/json';
  // Only changing requests carry the token; a GET never needs it.
  if (method !== 'GET' && csrfToken !== null) headers[CSRF_HEADER] = csrfToken;
  // The server rotates the token only for this header, which a navigation or an <img> of
  // another site cannot send.
  if (options.refresh) headers[CSRF_HEADER] = 'refresh';

  const response = await fetch(path, {
    method,
    headers,
    credentials: 'same-origin',
    body: options.body === undefined ? undefined : JSON.stringify(options.body),
  });
  const payload = await parse(response);

  if (response.ok) return options.envelope ? (payload as T) : ((payload as { data: T } | undefined)?.data as T);

  const error = toError(response.status, payload);
  if (error.status === 403 && error.code === 'csrf_failed' && method !== 'GET' && !options.retried) {
    await refreshCsrf();
    return request<T>(method, path, { ...options, retried: true });
  }
  if (error.status === 401 && !options.expectUnauthenticated) {
    csrfToken = null;
    onUnauthenticated?.();
  }
  throw error;
}

/** Logs in; the session cookie is set by the server, the CSRF token is kept in memory. */
export async function login(username: string, password: string): Promise<SessionInfo> {
  const session = await request<SessionInfo>('POST', '/api/v1/login', {
    body: { username, password },
    expectUnauthenticated: true,
  });
  csrfToken = session.csrf_token;
  return session;
}

/**
 * The user of the cookie session, with a **new** CSRF token (kept in memory). Rejects with a 401
 * `ApiError` when there is no session. Used on start and to recover from `csrf_failed`.
 */
export async function getSession(): Promise<SessionInfo> {
  const session = await request<SessionInfo>('GET', '/api/v1/session', {
    expectUnauthenticated: true,
    refresh: true,
  });
  csrfToken = session.csrf_token;
  return session;
}

export async function logout(): Promise<void> {
  try {
    await request<void>('POST', '/api/v1/logout', { expectUnauthenticated: true });
  } finally {
    csrfToken = null;
  }
}

export function getMeta(): Promise<DocTypeSummary[]> {
  return request<DocTypeSummary[]>('GET', '/api/v1/meta');
}

export function getDocTypeMeta(doctype: string): Promise<DocTypeMeta> {
  return request<DocTypeMeta>('GET', `/api/v1/meta/${encodeURIComponent(doctype)}`);
}

export interface ListParams {
  /** The `filters` JSON text (an array of `{field, op, value}`), see filters.ts. */
  filters?: string;
  /** `field` or `field:desc`. */
  sort?: string;
  limit: number;
  offset: number;
}

export function listDocuments(doctype: string, params: ListParams): Promise<DocumentRow[]> {
  const query = new URLSearchParams();
  if (params.filters) query.set('filters', params.filters);
  if (params.sort) query.set('sort', params.sort);
  query.set('limit', String(params.limit));
  query.set('offset', String(params.offset));
  return request<DocumentRow[]>(
    'GET',
    `/api/v1/resource/${encodeURIComponent(doctype)}?${query.toString()}`,
  );
}

/** A page of documents with the titles of the documents they link to (`link_titles=1`, UX.4). */
export interface TitledList {
  rows: DocumentRow[];
  titles: LinkTitles;
}

/** [`listDocuments`] that also asks for the titles of the linked documents the user may read. */
export async function listDocumentsWithTitles(doctype: string, params: ListParams): Promise<TitledList> {
  const query = new URLSearchParams();
  if (params.filters) query.set('filters', params.filters);
  if (params.sort) query.set('sort', params.sort);
  query.set('limit', String(params.limit));
  query.set('offset', String(params.offset));
  query.set('link_titles', '1');
  const answer = await request<{ data?: DocumentRow[]; link_titles?: unknown }>(
    'GET',
    `/api/v1/resource/${encodeURIComponent(doctype)}?${query.toString()}`,
    { envelope: true },
  );
  return { rows: answer?.data ?? [], titles: parseLinkTitles(answer?.link_titles) };
}

/**
 * The path of a document. A name that cannot be put in a path segment (`.`, `..`, empty: see
 * `isAddressable`) is refused here, so the desk never requests a wrong URL; the form view shows
 * a message for such a document before it gets this far.
 */
function documentPath(doctype: string, name: string): string {
  if (!isAddressable(name)) throw new Error(`the name ${JSON.stringify(name)} has no REST path`);
  return `/api/v1/resource/${encodeURIComponent(doctype)}/${encodeURIComponent(name)}`;
}

export function getDocument(doctype: string, name: string): Promise<DocumentRow> {
  return request<DocumentRow>('GET', documentPath(doctype, name));
}

/** A document with the titles of the documents it links to (`link_titles=1`, UX.4). */
export interface TitledDocument {
  doc: DocumentRow;
  titles: LinkTitles;
}

/** [`getDocument`] that also asks for the titles of the linked documents the user may read. */
export async function getDocumentWithTitles(doctype: string, name: string): Promise<TitledDocument> {
  const answer = await request<{ data: DocumentRow; link_titles?: unknown }>(
    'GET',
    `${documentPath(doctype, name)}?link_titles=1`,
    { envelope: true },
  );
  return { doc: answer.data, titles: parseLinkTitles(answer.link_titles) };
}

/** `POST`: creates a document (201); the answer is the stored document with its generated name. */
export function createDocument(doctype: string, data: Record<string, unknown>): Promise<DocumentRow> {
  return request<DocumentRow>('POST', `/api/v1/resource/${encodeURIComponent(doctype)}`, { body: data });
}

/** `PATCH`: only the changed fields in `data`; `version` is the one the user saw. */
export function updateDocument(
  doctype: string,
  name: string,
  version: number,
  data: Record<string, unknown>,
): Promise<DocumentRow> {
  return request<DocumentRow>('PATCH', documentPath(doctype, name), { body: { version, data } });
}

export function deleteDocument(doctype: string, name: string, version: number): Promise<void> {
  return request<void>('DELETE', `${documentPath(doctype, name)}?version=${version}`);
}

export type Transition = 'submit' | 'cancel' | 'amend';

/** Submit, cancel or amend (the answer of `amend` is the new draft). */
export function transitionDocument(
  doctype: string,
  name: string,
  transition: Transition,
  version: number,
): Promise<DocumentRow> {
  return request<DocumentRow>('POST', `${documentPath(doctype, name)}/${transition}`, {
    body: { version },
  });
}

/** An action of the workflow that the caller may take on a document now (`GET .../transitions`). */
export interface AvailableAction {
  action: string;
  to: string;
}

export interface AvailableActions {
  /** The document's workflow state, or null when it has none. */
  state: string | null;
  transitions: AvailableAction[];
}

/** The workflow actions this caller may take now (role and right applied; a hook may still veto). */
export function getTransitions(doctype: string, name: string): Promise<AvailableActions> {
  return request<AvailableActions>('GET', `${documentPath(doctype, name)}/transitions`);
}

/** Takes a workflow action; the answer is the stored document in its new state. */
export function workflowTransition(
  doctype: string,
  name: string,
  action: string,
  version: number,
): Promise<DocumentRow> {
  return request<DocumentRow>('POST', `${documentPath(doctype, name)}/transition`, {
    body: { action, version },
  });
}

/** One committed transition of the log. */
export interface HistoryEntry {
  action: string;
  from: string;
  to: string;
  actor: string;
  at: string;
}

/** The transitions of a document, oldest first, at most the newest 500. */
export async function getHistory(doctype: string, name: string): Promise<HistoryEntry[]> {
  const answer = await request<{ history?: HistoryEntry[] }>('GET', `${documentPath(doctype, name)}/history`);
  return answer?.history ?? [];
}
