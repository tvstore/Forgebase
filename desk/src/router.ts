// A tiny hash router: `#/login`, `#/`, `#/list/<doctype>?...`, `#/new/<doctype>` and
// `#/form/<doctype>/<name>`. State that belongs in the address (paging, sort, filters) is in the
// query part, so the back button and copied links work.

import { useSyncExternalStore } from 'react';

export type Route =
  | { name: 'login' }
  | { name: 'home' }
  | { name: 'list'; doctype: string; params: URLSearchParams }
  | { name: 'new'; doctype: string }
  | { name: 'form'; doctype: string; docName: string }
  | { name: 'notfound' };

function decode(segment: string): string | null {
  try {
    const value = decodeURIComponent(segment);
    return value === '' ? null : value;
  } catch {
    return null; // malformed percent-encoding
  }
}

/** Parses `location.hash` (with or without the leading `#`). */
export function parseHash(hash: string): Route {
  const text = hash.startsWith('#') ? hash.slice(1) : hash;
  if (text === '') return { name: 'home' };
  const [path = '', ...rest] = text.split('?');
  const query = rest.join('?');
  const parts = path.split('/');
  // A path starts with "/": parts[0] is "".
  if (parts[0] !== '') return { name: 'notfound' };
  const segments = parts.slice(1);
  if (segments.length === 1 && segments[0] === '') return { name: 'home' };
  const [kind, first, second, ...more] = segments;
  if (more.length > 0) return { name: 'notfound' };
  if (kind === 'login' && first === undefined) return { name: 'login' };
  if (kind === 'list' && first !== undefined && second === undefined) {
    const doctype = decode(first);
    return doctype === null
      ? { name: 'notfound' }
      : { name: 'list', doctype, params: new URLSearchParams(query) };
  }
  if (kind === 'new' && first !== undefined && second === undefined) {
    const doctype = decode(first);
    return doctype === null ? { name: 'notfound' } : { name: 'new', doctype };
  }
  if (kind === 'form' && first !== undefined && second !== undefined) {
    const doctype = decode(first);
    const docName = decode(second);
    return doctype === null || docName === null
      ? { name: 'notfound' }
      : { name: 'form', doctype, docName };
  }
  return { name: 'notfound' };
}

export const loginHref = (): string => '#/login';
export const homeHref = (): string => '#/';
export const newHref = (doctype: string): string => `#/new/${encodeURIComponent(doctype)}`;
export const formHref = (doctype: string, name: string): string =>
  `#/form/${encodeURIComponent(doctype)}/${encodeURIComponent(name)}`;

export function listHref(doctype: string, params?: URLSearchParams): string {
  const query = params?.toString() ?? '';
  return `#/list/${encodeURIComponent(doctype)}${query === '' ? '' : `?${query}`}`;
}

/** Goes to `href` (a `#/...` address); `replace` does not add a history entry (redirects). */
export function navigate(href: string, replace = false): void {
  if (replace) window.location.replace(href);
  else window.location.hash = href;
}

function subscribe(callback: () => void): () => void {
  window.addEventListener('hashchange', callback);
  return () => window.removeEventListener('hashchange', callback);
}

const snapshot = (): string => window.location.hash;

/** The hash of the address bar as a string (a primitive, so React can compare snapshots). */
export function useHash(): string {
  return useSyncExternalStore(subscribe, snapshot);
}
