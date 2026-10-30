// The command bar (Ctrl/Cmd+K): navigation only, computed on the client from what the desk already
// has (the DocTypes the user may read, from /api/v1/meta, and the documents opened in this tab).
// It searches no document content and asks the server for nothing.

import type { RecentDocument } from './recent';
import { formHref, listHref, newHref } from './router';
import type { DocTypeSummary } from './types';

/** The most results shown. */
export const MAX_RESULTS = 20;

export type CommandKind = 'recent' | 'doctype' | 'create';

export interface Command {
  /** Unique in a result list (the id of the option element is built from it). */
  id: string;
  kind: CommandKind;
  /** The main text: "Go to Sales Order", "New Sales Order" or the document's title. */
  label: string;
  /** The muted text at the end: the DocType of a recent document. */
  hint?: string;
  /** The address to open. */
  href: string;
}

export interface CommandGroup {
  title: 'Recent' | 'DocTypes' | 'Create';
  commands: Command[];
}

const matches = (query: string, ...texts: string[]): boolean =>
  query === '' || texts.some((text) => text.toLowerCase().includes(query));

/**
 * The results for `input`, grouped (Recent, DocTypes, Create) and cut at [`MAX_RESULTS`] in that
 * order. Matching is a case-insensitive substring of the DocType's label (and name), of a recent
 * document's title and name. "New <DocType>" is offered only where the user has the create right.
 * With nothing typed, everything is listed (up to the cap).
 */
export function findCommands(
  input: string,
  doctypes: readonly DocTypeSummary[],
  recent: readonly RecentDocument[],
): CommandGroup[] {
  const query = input.trim().toLowerCase();
  const label = new Map(doctypes.map((d) => [d.name, d.label]));
  const groups: CommandGroup[] = [
    {
      title: 'Recent',
      commands: recent
        .filter((r) => label.has(r.doctype) && matches(query, r.title, r.name))
        .map((r) => ({
          id: `recent/${r.doctype}/${r.name}`,
          kind: 'recent' as const,
          label: r.title,
          hint: label.get(r.doctype) ?? r.doctype,
          href: formHref(r.doctype, r.name),
        })),
    },
    {
      title: 'DocTypes',
      commands: doctypes
        .filter((d) => matches(query, d.label, d.name))
        .map((d) => ({ id: `doctype/${d.name}`, kind: 'doctype' as const, label: `Go to ${d.label}`, href: listHref(d.name) })),
    },
    {
      title: 'Create',
      commands: doctypes
        .filter((d) => d.rights.create !== 'none' && matches(query, d.label, d.name))
        .map((d) => ({ id: `create/${d.name}`, kind: 'create' as const, label: `New ${d.label}`, href: newHref(d.name) })),
    },
  ];
  let left = MAX_RESULTS;
  const capped: CommandGroup[] = [];
  for (const group of groups) {
    const commands = group.commands.slice(0, left);
    left -= commands.length;
    if (commands.length > 0) capped.push({ title: group.title, commands });
  }
  return capped;
}

/** The commands of all groups in display order (the order of the arrow keys). */
export function flatten(groups: readonly CommandGroup[]): Command[] {
  return groups.flatMap((g) => g.commands);
}

/** The next active index after an arrow key: it wraps around at both ends; -1 when there is nothing. */
export function moveActive(current: number, delta: 1 | -1, count: number): number {
  if (count === 0) return -1;
  if (current < 0) return delta === 1 ? 0 : count - 1;
  return (current + delta + count) % count;
}
