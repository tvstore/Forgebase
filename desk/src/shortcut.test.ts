import { describe, expect, it } from 'vitest';

import { saveShortcut } from './shortcut';

const key = (k: string, extra: Partial<Parameters<typeof saveShortcut>[0]> = {}) => ({
  key: k,
  ctrlKey: false,
  metaKey: false,
  altKey: false,
  shiftKey: false,
  ...extra,
});
const dirty = { canSave: true, dirty: true, isNew: false, dialogOpen: false };

describe('saveShortcut', () => {
  it('saves on Ctrl+S and Cmd+S when there is something to save', () => {
    expect(saveShortcut(key('s', { ctrlKey: true }), dirty)).toBe('save');
    expect(saveShortcut(key('S', { metaKey: true }), dirty)).toBe('save');
    expect(saveShortcut(key('s', { ctrlKey: true }), { canSave: true, dirty: false, isNew: true, dialogOpen: false })).toBe('save');
  });

  it('swallows the browser dialog but does nothing when there are no changes', () => {
    expect(saveShortcut(key('s', { ctrlKey: true }), { canSave: true, dirty: false, isNew: false, dialogOpen: false })).toBe('swallow');
  });

  it('does not save while a dialog is open (and still stops the browser dialog)', () => {
    expect(saveShortcut(key('s', { ctrlKey: true }), { ...dirty, dialogOpen: true })).toBe('swallow');
    expect(saveShortcut(key('s', { metaKey: true }), { canSave: true, dirty: false, isNew: true, dialogOpen: true })).toBe('swallow');
    expect(saveShortcut(key('s', { ctrlKey: true }), { ...dirty, canSave: false, dialogOpen: true })).toBe('ignore');
  });

  it('ignores the key when the user cannot save, and every other key', () => {
    expect(saveShortcut(key('s', { ctrlKey: true }), { canSave: false, dirty: true, isNew: false, dialogOpen: false })).toBe('ignore');
    expect(saveShortcut(key('s'), dirty)).toBe('ignore');
    expect(saveShortcut(key('a', { ctrlKey: true }), dirty)).toBe('ignore');
    expect(saveShortcut(key('s', { ctrlKey: true, shiftKey: true }), dirty)).toBe('ignore');
    expect(saveShortcut(key('s', { ctrlKey: true, altKey: true }), dirty)).toBe('ignore');
  });
});
