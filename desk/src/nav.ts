// Pure helpers of the navigation: the DocTypes grouped by app (sidebar, home) and the breadcrumbs.

import type { Route } from './router';
import { homeHref, listHref } from './router';
import type { DocTypeSummary } from './types';

/** The group of a DocType whose app is not recorded. */
export const OTHER_APP = 'Other';

/** The name of an app as people read it: `sales_extension` is "Sales Extension". */
export function appLabel(app: string | null): string {
  if (app === null || app === '') return OTHER_APP;
  return app
    .split(/[_\-\s]+/)
    .filter((word) => word !== '')
    .map((word) => word.charAt(0).toUpperCase() + word.slice(1))
    .join(' ');
}

export interface AppGroup {
  /** The app as recorded (the sort key); "Other" when none is. */
  app: string;
  /** The app as shown. */
  label: string;
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
    .map(([app, list]) => ({ app, label: appLabel(app), doctypes: [...list].sort((a, b) => byText(a.label, b.label)) }));
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
  if (known) crumbs.push({ label: appLabel(known.app), href: homeHref() });
  const label = known?.label ?? route.doctype;
  if (route.name === 'list') return [...crumbs, { label }];
  crumbs.push({ label, href: listHref(route.doctype) });
  if (route.name === 'new') return [...crumbs, { label: 'New' }];
  return [...crumbs, { label: route.docName }];
}
