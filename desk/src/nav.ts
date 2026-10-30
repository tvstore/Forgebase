// Pure helpers of the navigation: the DocTypes grouped by app (sidebar, home) and the breadcrumbs.

import type { Route } from './router';
import { homeHref, listHref } from './router';
import type { DocTypeSummary } from './types';

/** The group of a DocType whose app is not recorded. */
export const OTHER_APP = 'Other';

export interface AppGroup {
  app: string;
  doctypes: DocTypeSummary[];
}

const byText = (a: string, b: string): number => a.localeCompare(b, 'en', { sensitivity: 'base' });

/** DocTypes grouped by app: apps A to Z with "Other" last, DocTypes by label within a group. */
export function groupByApp(doctypes: DocTypeSummary[]): AppGroup[] {
  const groups = new Map<string, DocTypeSummary[]>();
  for (const d of doctypes) {
    const app = d.app === null || d.app === '' ? OTHER_APP : d.app;
    groups.set(app, [...(groups.get(app) ?? []), d]);
  }
  return [...groups.entries()]
    .sort(([a], [b]) => (a === OTHER_APP ? 1 : b === OTHER_APP ? -1 : byText(a, b)))
    .map(([app, list]) => ({ app, doctypes: [...list].sort((a, b) => byText(a.label, b.label)) }));
}

export interface Crumb {
  label: string;
  /** Absent for the page the user is on. */
  href?: string;
}

/** App / DocType / Document for a list, new or form route; nothing for the other routes. */
export function breadcrumbs(route: Route, doctypes: DocTypeSummary[]): Crumb[] {
  if (route.name !== 'list' && route.name !== 'new' && route.name !== 'form') return [];
  const known = doctypes.find((d) => d.name === route.doctype);
  const crumbs: Crumb[] = [];
  if (known) crumbs.push({ label: known.app ?? OTHER_APP, href: homeHref() });
  const label = known?.label ?? route.doctype;
  if (route.name === 'list') return [...crumbs, { label }];
  crumbs.push({ label, href: listHref(route.doctype) });
  if (route.name === 'new') return [...crumbs, { label: 'New' }];
  return [...crumbs, { label: route.docName }];
}
