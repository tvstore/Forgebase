// The fetch wrapper of the desk: JSON in and out, the error envelope as a typed error and the
// CSRF header. The CSRF token is kept in this module's memory only: it is never written to any
// browser storage (the eslint config forbids it). After a page reload it is recovered with
// `GET /api/v1/session`, which issues a new one; a request refused with `csrf_failed` (the token
// was rotated by another tab) refreshes it once and is retried once.

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

  if (response.ok) return (payload as { data: T } | undefined)?.data as T;

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
