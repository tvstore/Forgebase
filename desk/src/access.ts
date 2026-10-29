// Pure rules of the form view: which controls are read-only, which lifecycle buttons are shown
// and which names the REST path cannot address. The rights come from `GET /api/v1/meta/{doctype}`
// and are **advisory**: the desk only hides what the API would refuse anyway, and the API still
// enforces every right (an exit criterion of M4: hidden in the UI and refused by the API).

import type { Access, DocTypeMeta, FieldMeta, Rights } from './types';

/** 0 draft, 1 submitted, 2 cancelled. A DocType that is not submittable has only drafts. */
export type DocStatus = 0 | 1 | 2;

export const docStatusOf = (value: unknown): DocStatus => (value === 1 || value === 2 ? value : 0);

export const docStatusLabel = (status: DocStatus): string =>
  status === 1 ? 'Submitted' : status === 2 ? 'Cancelled' : 'Draft';

/**
 * Whether a right applies to a document: `all`, or `own` for a document the user owns. For
 * `own` the desk acts only when it knows the owner is the user (the server decides in every
 * case, so this only keeps the UI from offering what it would refuse).
 */
export function permits(access: Access, owner: string | null, username: string): boolean {
  if (access === 'all') return true;
  if (access === 'own') return owner !== null && owner === username;
  return false;
}

/** What the read-only rules need to know about the open document. */
export interface DocContext {
  /** True on `#/new/<doctype>`. */
  isNew: boolean;
  docStatus: DocStatus;
  rights: Rights;
  /** The document's owner (null for a new document). */
  owner: string | null;
  username: string;
  submittable: boolean;
}

/**
 * Whether the user may change the document at all: create on a new one; write on an existing
 * one (`own` only for a document the user owns, see {@link permits}).
 */
export function mayChange(ctx: DocContext): boolean {
  return ctx.isNew ? ctx.rights.create !== 'none' : permits(ctx.rights.write, ctx.owner, ctx.username);
}

/**
 * Whether a field is read-only. A draft is editable by a user who may change it; a submitted
 * document only in its `allow_on_submit` fields (never its tables); a cancelled one not at all;
 * the field that names the document only while it is created (renames are not supported).
 */
export function fieldReadOnly(ctx: DocContext, meta: DocTypeMeta, field: FieldMeta): boolean {
  if (!mayChange(ctx)) return true;
  if (!ctx.isNew && meta.naming === `field:${field.name}`) return true;
  if (ctx.isNew || ctx.docStatus === 0) return false;
  if (ctx.docStatus === 2) return true;
  return field.type === 'table' || !field.allow_on_submit;
}

/** True when at least one field of the document can be edited (so the Save button is shown). */
export function hasEditableField(ctx: DocContext, meta: DocTypeMeta): boolean {
  return meta.fields.some((f) => !fieldReadOnly(ctx, meta, f));
}

export type LifecycleAction = 'submit' | 'cancel' | 'amend' | 'delete';

/**
 * The lifecycle buttons to show, in order. Nothing is shown for a new document. Submit: a draft
 * of a submittable DocType and the submit right. Cancel: a submitted document and the cancel
 * right. Amend: a cancelled document and the amend right. Delete: a draft or a cancelled
 * document (any document of a DocType that is not submittable) and the delete right. The rights
 * for submit, cancel and amend are already `none` for a DocType that is not submittable.
 */
export function lifecycleActions(ctx: DocContext): LifecycleAction[] {
  if (ctx.isNew) return [];
  const { rights, owner, username } = ctx;
  const actions: LifecycleAction[] = [];
  if (ctx.submittable && ctx.docStatus === 0 && permits(rights.submit, owner, username)) {
    actions.push('submit');
  }
  if (ctx.submittable && ctx.docStatus === 1 && permits(rights.cancel, owner, username)) {
    actions.push('cancel');
  }
  if (ctx.submittable && ctx.docStatus === 2 && permits(rights.amend, owner, username)) {
    actions.push('amend');
  }
  if (ctx.docStatus !== 1 && permits(rights.delete, owner, username)) actions.push('delete');
  return actions;
}

/**
 * Whether a document name can be put in a REST path. A path segment `.` or `..` is resolved by
 * the browser before the request is sent (`/resource/x/..` becomes `/resource`), so such a
 * document cannot be opened through the REST API, and an empty name has no path at all. Every
 * other name is `encodeURIComponent`-encoded. A documented limit (architecture, section 11).
 */
export function isAddressable(name: string): boolean {
  return name !== '' && name !== '.' && name !== '..';
}
