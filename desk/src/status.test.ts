import { describe, expect, it } from 'vitest';

import { statusBadge } from './status';

describe('statusBadge', () => {
  it('maps the document status to a text and a tone', () => {
    expect(statusBadge(0)).toEqual({ label: 'Draft', tone: 'neutral' });
    expect(statusBadge(1)).toEqual({ label: 'Submitted', tone: 'success' });
    expect(statusBadge(2)).toEqual({ label: 'Cancelled', tone: 'danger' });
  });

  it('treats a missing or unknown status as a draft', () => {
    expect(statusBadge(undefined).label).toBe('Draft');
    expect(statusBadge(7).label).toBe('Draft');
  });
});
