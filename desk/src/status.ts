// The status badge of a document: the text always says the status, the colour only supports it.

import { docStatusLabel, docStatusOf } from './access';

export type Tone = 'neutral' | 'success' | 'danger' | 'warning' | 'info';

export interface BadgeSpec {
  label: string;
  tone: Tone;
}

/** Draft is grey, Submitted green, Cancelled red. */
export function statusBadge(docstatus: unknown): BadgeSpec {
  const status = docStatusOf(docstatus);
  return { label: docStatusLabel(status), tone: status === 1 ? 'success' : status === 2 ? 'danger' : 'neutral' };
}
