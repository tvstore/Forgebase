// The Ctrl/Cmd+S shortcut of the form, without a DOM so that it can be tested.

export interface KeyLike {
  key: string;
  ctrlKey: boolean;
  metaKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
}

/** What the form does with a key press. */
export type ShortcutAction = 'save' | 'swallow' | 'ignore';

export interface ShortcutState {
  canSave: boolean;
  dirty: boolean;
  isNew: boolean;
  /** A question (dialog) is open: it is answered first, nothing is saved behind it. */
  dialogOpen: boolean;
}

/**
 * Ctrl+S or Cmd+S: `save` when the document is new or has unsaved changes and may be saved;
 * `swallow` (stop the browser's own "save page" dialog, do nothing) when it may be saved but has
 * no changes or a dialog is open; `ignore` for every other key and when the user cannot save at all.
 */
export function saveShortcut(event: KeyLike, state: ShortcutState): ShortcutAction {
  const isSave = event.key.toLowerCase() === 's' && (event.ctrlKey || event.metaKey) && !event.altKey && !event.shiftKey;
  if (!isSave || !state.canSave) return 'ignore';
  if (state.dialogOpen) return 'swallow';
  return state.dirty || state.isNew ? 'save' : 'swallow';
}
