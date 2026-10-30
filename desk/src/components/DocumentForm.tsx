import { useEffect, useId, useMemo, useRef, useState } from 'react';
import type { FormEvent, ReactNode } from 'react';

import { ApiError, createDocument, deleteDocument, getDocument, transitionDocument, updateDocument } from '../api';
import type { Transition } from '../api';
import { docStatusLabel, docStatusOf, fieldReadOnly, hasEditableField, hasEditableRequired, headerActions } from '../access';
import type { DocContext, LifecycleAction } from '../access';
import { conflictMessage, isStaleCode, mapDetails } from '../errors';
import { blankState, buildChanges, buildCreate, changedFields, errorKey, stateOf } from '../fields';
import type { Errors, FormState, FormValue } from '../fields';
import { formHref, listHref, navigate, setNavigationBlocker } from '../router';
import { formatCell, rowTitle } from '../listing';
import { saveShortcut } from '../shortcut';
import { statusBadge } from '../status';
import type { DocTypeMeta, DocumentRow, FieldMeta } from '../types';
import { Badge } from './Badge';
import { Dialog } from './Dialog';
import { FieldControl } from './FieldControl';
import { MenuButton } from './MenuButton';
import { TableGrid } from './TableGrid';
import { useToast } from './Toast';

interface Props {
  meta: DocTypeMeta;
  /** The stored document, or null for a new one. */
  initial: DocumentRow | null;
  username: string;
}

/** The question the form is asking in the page, if any. */
type Asking =
  | { kind: 'delete' }
  | { kind: 'cancel' }
  | { kind: 'amend' }
  | { kind: 'reload' }
  | { kind: 'leave'; next: string }
  | null;

/** An error that is not about a field: shown in the alert at the top. */
interface Failure {
  message: string;
  details: string[];
}

const TRANSITION_VERB: Record<Transition, string> = {
  submit: 'submitted',
  cancel: 'cancelled',
  amend: 'amended',
};

const TRANSITION_DONE: Record<Transition, string> = {
  submit: 'Submitted',
  cancel: 'Cancelled',
  amend: 'Amended',
};

const ACTION_LABEL: Record<LifecycleAction, string> = {
  submit: 'Submit',
  cancel: 'Cancel',
  amend: 'Amend',
  delete: 'Delete',
};

/**
 * The form of one document: a native control per field of the metadata, Save, the lifecycle
 * buttons the user's rights and the document's status allow, and the messages. The state of the
 * controls is `FormState` (strings and booleans); the conversion to JSON and the change
 * detection are in fields.ts and the rules in access.ts (both tested without a DOM).
 */
export function DocumentForm({ meta, initial, username }: Props) {
  const [doc, setDoc] = useState<DocumentRow | null>(initial);
  const [saved, setSaved] = useState<FormState>(() => (initial ? stateOf(meta, initial) : blankState(meta)));
  const [state, setState] = useState<FormState>(saved);
  const [errors, setErrors] = useState<Errors>({});
  const [failure, setFailure] = useState<Failure | null>(null);
  const toast = useToast();
  const [busy, setBusy] = useState(false);
  const [asking, setAsking] = useState<Asking>(null);
  // The version in the form is known to be out of date (a conflict, or a status that moved on):
  // Reload stays offered until the current version is loaded.
  const [stale, setStale] = useState(false);
  // Counts the attempts that failed, so the focus moves each time, also to the same field.
  const [attempt, setAttempt] = useState(0);

  const idBase = useId();
  const alertRef = useRef<HTMLDivElement>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const formRef = useRef<HTMLFormElement>(null);

  const isNew = doc === null;
  const ctx: DocContext = {
    isNew,
    docStatus: docStatusOf(doc?.docstatus),
    rights: meta.rights,
    owner: typeof doc?.owner === 'string' ? doc.owner : null,
    username,
    submittable: meta.submittable,
  };
  const readOnlyOf = (field: FieldMeta): boolean => fieldReadOnly(ctx, meta, field);
  const changed = useMemo(() => changedFields(meta, saved, state), [meta, saved, state]);
  const dirty = changed.length > 0;
  const canSave = hasEditableField(ctx, meta);
  const header = headerActions(ctx, dirty, canSave);
  const name = typeof doc?.name === 'string' ? doc.name : null;
  const version = typeof doc?.version === 'number' ? doc.version : 0;

  // The form heading takes the focus when the page opens, so a screen reader announces where the
  // user arrived (the page is replaced without a page load).
  useEffect(() => {
    headingRef.current?.focus();
  }, []);

  // After a failed attempt: the first invalid field, else the alert.
  useEffect(() => {
    if (attempt === 0) return;
    const invalid = formRef.current?.querySelector<HTMLElement>('[aria-invalid="true"]');
    (invalid ?? alertRef.current)?.focus();
  }, [attempt]);

  // Unsaved changes: leaving by a change of the address asks first (in the page); closing or
  // reloading the tab uses the browser's own prompt.
  const dirtyRef = useRef(false);
  const leavingRef = useRef(false);
  useEffect(() => {
    dirtyRef.current = dirty;
  });
  useEffect(() => {
    setNavigationBlocker((next) => {
      if (leavingRef.current || !dirtyRef.current) return false;
      setAsking({ kind: 'leave', next });
      return true;
    });
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (dirtyRef.current && !leavingRef.current) event.preventDefault();
    };
    window.addEventListener('beforeunload', beforeUnload);
    return () => {
      setNavigationBlocker(null);
      window.removeEventListener('beforeunload', beforeUnload);
    };
  }, []);

  const leaveTo = (href: string) => {
    leavingRef.current = true;
    navigate(href);
  };

  const adopt = (next: DocumentRow) => {
    const fresh = stateOf(meta, next);
    setDoc(next);
    setSaved(fresh);
    setState(fresh);
    setErrors({});
    setFailure(null);
    setStale(false);
  };

  /** `verb` completes "The document was not ..." (saved, submitted, ...). */
  const fail = (e: unknown, verb: string) => {
    setAsking(null);
    if (e instanceof ApiError) {
      if (e.status === 422 && e.code === 'validation_failed') {
        const mapped = mapDetails(e.details, meta);
        setErrors(mapped.errors);
        setFailure({ message: `The document was not ${verb}.`, details: mapped.unmapped });
      } else if (e.code === 'conflict') {
        setErrors({});
        setStale(true);
        setFailure({ message: conflictMessage(verb, dirty), details: [] });
      } else {
        setErrors({});
        if (isStaleCode(e.code)) setStale(true);
        setFailure({ message: e.message, details: e.details });
      }
    } else {
      setErrors({});
      setFailure({ message: 'Cannot reach the server. Try again.', details: [] });
    }
    setAttempt((n) => n + 1);
  };

  const run = async (verb: string, work: () => Promise<void>) => {
    setBusy(true);
    setFailure(null);
    setErrors({});
    try {
      await work();
    } catch (e) {
      fail(e, verb);
    } finally {
      setBusy(false);
    }
  };

  const save = (event?: FormEvent) => {
    event?.preventDefault();
    if (busy || !canSave) return;
    const built = isNew
      ? buildCreate(meta, state)
      : buildChanges(meta, saved, state, (f) => !readOnlyOf(f));
    if (!built.ok) {
      setErrors(built.errors);
      setFailure({ message: 'Fix the fields marked below. The document was not saved.', details: [] });
      setAttempt((n) => n + 1);
      return;
    }
    if (!isNew && Object.keys(built.data).length === 0) {
      toast('There are no changes to save.');
      return;
    }
    void run('saved', async () => {
      if (name === null) {
        const created = await createDocument(meta.name, built.data);
        leavingRef.current = true;
        toast('Saved');
        navigate(formHref(meta.name, String(created.name)));
      } else {
        const updated = await updateDocument(meta.name, name, version, built.data);
        adopt(updated);
        toast('Saved');
      }
    });
  };

  const transition = (action: Transition) => {
    if (busy || name === null) return;
    setAsking(null);
    void run(TRANSITION_VERB[action], async () => {
      const result = await transitionDocument(meta.name, name, action, version);
      toast(TRANSITION_DONE[action]);
      if (action === 'amend') {
        leavingRef.current = true;
        navigate(formHref(meta.name, String(result.name)));
        return;
      }
      adopt(result);
      // The button that was used is gone: put the focus on the heading, which names the document.
      headingRef.current?.focus();
    });
  };

  /** Submit goes ahead; Cancel, Amend and Delete ask first. With unsaved changes all ask to save first. */
  const lifecycle = (action: LifecycleAction) => {
    if (busy || name === null) return;
    if (dirty) {
      setErrors({});
      setFailure({ message: 'You have unsaved changes. Save them first.', details: [] });
      setAttempt((n) => n + 1);
      return;
    }
    if (action === 'submit') transition('submit');
    else setAsking({ kind: action });
  };

  const confirmDelete = () => {
    if (name === null) return;
    setAsking(null);
    void run('deleted', async () => {
      await deleteDocument(meta.name, name, version);
      leavingRef.current = true;
      navigate(listHref(meta.name));
    });
  };

  const reload = () => {
    if (name === null) return;
    setAsking(null);
    void run('reloaded', async () => {
      adopt(await getDocument(meta.name, name));
      toast('Loaded the current version');
      // The Reload button is gone: put the focus on the heading, as after a lifecycle action.
      headingRef.current?.focus();
    });
  };

  // Ctrl/Cmd+S saves (the browser's own "save page" dialog is not wanted here).
  const saveCallback = useRef<() => void>(() => {});
  useEffect(() => {
    saveCallback.current = () => save();
  });
  const shortcutState = { canSave, dirty, isNew, dialogOpen: asking !== null };
  const shortcutRef = useRef(shortcutState);
  useEffect(() => {
    shortcutRef.current = shortcutState;
  });
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      const action = saveShortcut(event, shortcutRef.current);
      if (action === 'ignore') return;
      event.preventDefault();
      if (action === 'save') saveCallback.current();
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  const setValue = (field: string, value: FormValue) => {
    setState((s) => ({ ...s, values: { ...s.values, [field]: value } }));
    setErrors((e) => (field in e ? Object.fromEntries(Object.entries(e).filter(([k]) => k !== field)) : e));
  };

  // The heading is the title field's value (as in the list), the name muted under it.
  const shown = doc === null ? null : rowTitle(doc, meta);
  const title = shown === null ? `New ${meta.label}` : shown.title === '' ? meta.label : shown.title;
  const docName = name ?? 'this document';

  // Problems that no single control carries: shown in the alert next to the unmapped details.
  const tableLevel = meta.fields
    .filter((f) => f.type === 'table' && errors[errorKey(f.name)] !== undefined)
    .map((f) => `${f.label}: ${errors[errorKey(f.name)]}`);

  const badge = statusBadge(doc?.docstatus);
  const requiredShown = canSave && hasEditableRequired(ctx, meta);

  return (
    <div className="page">
      <p className="visually-hidden" role="status" aria-live="polite">
        {busy ? 'Working...' : ''}
      </p>
      <form ref={formRef} noValidate onSubmit={save}>
        <header className="doc-head">
          <div className="doc-title">
            <h1 ref={headingRef} tabIndex={-1}>
              {title}
            </h1>
            {meta.submittable && !isNew ? (
              <Badge tone={badge.tone} className="status-badge">
                {badge.label}
              </Badge>
            ) : null}
            {isNew || dirty ? <Badge tone="warning">{isNew ? 'Not saved' : 'Unsaved changes'}</Badge> : null}
            {shown !== null && shown.title !== shown.name ? <p className="doc-title-name">{shown.name}</p> : null}
          </div>
          <div className="doc-actions">
            {header.save ? (
              <button
                type="submit"
                className={header.save === 'primary' ? 'btn btn-primary' : 'btn'}
                aria-disabled={busy || undefined}
              >
                Save
              </button>
            ) : null}
            {header.submit ? (
              <button
                type="button"
                className={header.submit === 'primary' ? 'btn btn-primary' : 'btn'}
                aria-disabled={busy || undefined}
                onClick={() => lifecycle('submit')}
              >
                Submit
              </button>
            ) : null}
            {header.more.length > 0 ? (
              <MenuButton label="More" ariaLabel="More actions">
                {header.more.map((action) => (
                  <button
                    key={action}
                    type="button"
                    className={action === 'delete' ? 'menu-item menu-danger' : 'menu-item'}
                    aria-disabled={busy || undefined}
                    onClick={() => lifecycle(action)}
                  >
                    {ACTION_LABEL[action]}
                  </button>
                ))}
              </MenuButton>
            ) : null}
          </div>
        </header>

        {failure ? (
          <div role="alert" tabIndex={-1} ref={alertRef} className="alert">
            <p>{failure.message}</p>
            {failure.details.length + tableLevel.length > 0 ? (
              <ul>
                {[...failure.details, ...tableLevel].map((d) => (
                  <li key={d}>{d}</li>
                ))}
              </ul>
            ) : null}
            {stale ? (
              <button
                type="button"
                className="btn btn-small"
                onClick={() => (dirty ? setAsking({ kind: 'reload' }) : reload())}
              >
                Reload
              </button>
            ) : null}
          </div>
        ) : null}

        {asking?.kind === 'delete' ? (
          <Dialog
            title={`Delete ${docName}?`}
            confirmLabel="Delete"
            cancelLabel="Keep"
            danger
            onConfirm={confirmDelete}
            onCancel={() => setAsking(null)}
          >
            This cannot be undone.
          </Dialog>
        ) : null}
        {asking?.kind === 'cancel' ? (
          <Dialog
            title={`Cancel ${docName}?`}
            confirmLabel="Cancel document"
            cancelLabel="Go back"
            danger
            onConfirm={() => transition('cancel')}
            onCancel={() => setAsking(null)}
          >
            The document is marked Cancelled and can no longer be changed. You can amend it afterwards.
          </Dialog>
        ) : null}
        {asking?.kind === 'amend' ? (
          <Dialog
            title={`Amend ${docName}?`}
            confirmLabel="Amend"
            cancelLabel="Go back"
            onConfirm={() => transition('amend')}
            onCancel={() => setAsking(null)}
          >
            A new draft is created from this cancelled document.
          </Dialog>
        ) : null}
        {asking?.kind === 'reload' ? (
          <Dialog
            title="Reload the current version?"
            confirmLabel="Discard edits and reload"
            cancelLabel="Keep editing"
            danger
            onConfirm={reload}
            onCancel={() => setAsking(null)}
          >
            Reloading discards your unsaved edits.
          </Dialog>
        ) : null}
        {asking?.kind === 'leave' ? (
          <Dialog
            title="You have unsaved changes."
            confirmLabel="Leave and discard"
            cancelLabel="Stay"
            danger
            onConfirm={() => leaveTo(asking.next)}
            onCancel={() => setAsking(null)}
          >
            Leave this page and lose them?
          </Dialog>
        ) : null}

        {isNew && !canSave ? (
          <p role="alert" className="alert">
            You cannot create {meta.label} documents.
          </p>
        ) : (
          <section className="card form-card" aria-label={`${meta.label} fields`}>
            {requiredShown ? <p className="hint required-hint">* required</p> : null}
            <div className="fields">
              {meta.fields.map((field) => {
                const id = `${idBase}-${field.name}`;
                const message = errors[errorKey(field.name)];
                const readOnly = readOnlyOf(field);
                if (field.type === 'table') {
                  return (
                    <fieldset key={field.name} className="field-wide table-field">
                      <legend>
                        {field.label}
                        {field.required && !readOnly ? <span aria-hidden="true"> *</span> : null}
                      </legend>
                      <TableGrid
                        field={field}
                        rows={state.tables[field.name] ?? []}
                        onChange={(rows) => {
                          setState((s) => ({ ...s, tables: { ...s.tables, [field.name]: rows } }));
                          setErrors((e) =>
                            Object.fromEntries(
                              Object.entries(e).filter(([k]) => k !== field.name && !k.startsWith(`${field.name}/`)),
                            ),
                          );
                        }}
                        readOnly={readOnly}
                        errors={errors}
                        idBase={id}
                      />
                    </fieldset>
                  );
                }
                const control = (
                  <FieldControl
                    id={id}
                    field={field}
                    value={state.values[field.name] ?? ''}
                    onChange={(value) => setValue(field.name, value)}
                    readOnly={readOnly}
                    invalid={message !== undefined}
                    describedBy={message ? `${id}-error` : undefined}
                  />
                );
                const label = (
                  <label htmlFor={id}>
                    {field.label}
                    {field.required && !readOnly ? <span aria-hidden="true"> *</span> : null}
                  </label>
                );
                const classes = [
                  'field',
                  field.type === 'check' ? 'field-check' : '',
                  field.type === 'text' ? 'field-wide' : '',
                  readOnly ? 'is-readonly' : '',
                ]
                  .filter(Boolean)
                  .join(' ');
                return (
                  <div key={field.name} className={classes}>
                    {field.type === 'check' ? (
                      <>
                        {control}
                        {label}
                      </>
                    ) : (
                      <>
                        {label}
                        {control}
                      </>
                    )}
                    {message ? (
                      <p id={`${id}-error`} className="field-error">
                        {message}
                      </p>
                    ) : null}
                  </div>
                );
              })}
            </div>
          </section>
        )}
      </form>

      {doc ? <Details meta={meta} doc={doc} /> : null}
    </div>
  );
}

/** The standard columns, read-only. */
function Details({ meta, doc }: { meta: DocTypeMeta; doc: DocumentRow }) {
  const rows: [string, ReactNode][] = [
    ['Owner', String(doc.owner ?? '')],
    ['Created', formatCell(doc.created_at, 'datetime')],
    ['Modified by', String(doc.modified_by ?? '')],
    ['Modified', formatCell(doc.modified_at, 'datetime')],
    ['Version', String(doc.version ?? '')],
  ];
  if (meta.submittable) {
    rows.push(['Document status', docStatusLabel(docStatusOf(doc.docstatus))]);
    if (typeof doc.amended_from === 'string') {
      rows.push([
        'Amended from',
        <a key="a" href={formHref(meta.name, doc.amended_from)}>
          {doc.amended_from}
        </a>,
      ]);
    }
  }
  return (
    <section aria-labelledby="details-heading" className="card details">
      <h2 id="details-heading">Details</h2>
      <dl>
        {rows.map(([term, value]) => (
          <div key={term}>
            <dt>{term}</dt>
            <dd>{value}</dd>
          </div>
        ))}
      </dl>
    </section>
  );
}
