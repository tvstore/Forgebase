import { useEffect, useId, useMemo, useRef, useState } from 'react';
import type { FormEvent, ReactNode } from 'react';

import { ApiError, createDocument, deleteDocument, getDocument, transitionDocument, updateDocument } from '../api';
import type { Transition } from '../api';
import { docStatusLabel, docStatusOf, fieldReadOnly, hasEditableField, lifecycleActions } from '../access';
import type { DocContext, LifecycleAction } from '../access';
import { mapDetails } from '../errors';
import { blankState, buildChanges, buildCreate, changedFields, errorKey, stateOf } from '../fields';
import type { Errors, FormState, FormValue } from '../fields';
import { formHref, listHref, navigate, setNavigationBlocker } from '../router';
import { formatCell } from '../listing';
import type { DocTypeMeta, DocumentRow, FieldMeta } from '../types';
import { FieldControl } from './FieldControl';
import { Prompt } from './Prompt';
import { TableGrid } from './TableGrid';

interface Props {
  meta: DocTypeMeta;
  /** The stored document, or null for a new one. */
  initial: DocumentRow | null;
  username: string;
}

/** The question the form is asking in the page, if any. */
type Asking =
  | { kind: 'delete' }
  | { kind: 'reload' }
  | { kind: 'leave'; next: string }
  | null;

/** An error that is not about a field: shown in the alert at the top. */
interface Failure {
  message: string;
  details: string[];
  /** The document changed under the user: offer to load the current version. */
  reload: boolean;
}

const TRANSITION_VERB: Record<Transition, string> = {
  submit: 'submitted',
  cancel: 'cancelled',
  amend: 'amended',
};

const TRANSITION_DONE: Record<Transition, string> = {
  submit: 'Submitted.',
  cancel: 'Cancelled.',
  amend: 'Amended.',
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
  const [status, setStatus] = useState('');
  const [busy, setBusy] = useState(false);
  const [asking, setAsking] = useState<Asking>(null);
  // Counts the attempts that failed, so the focus moves each time, also to the same field.
  const [attempt, setAttempt] = useState(0);

  const idBase = useId();
  const alertRef = useRef<HTMLDivElement>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const formRef = useRef<HTMLFormElement>(null);
  const saveRef = useRef<HTMLButtonElement>(null);

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
  const actions = lifecycleActions(ctx);
  const canSave = hasEditableField(ctx, meta);
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
  };

  /** `verb` completes "The document was not ..." (saved, submitted, ...). */
  const fail = (e: unknown, verb: string) => {
    setStatus('');
    setAsking(null);
    if (e instanceof ApiError) {
      if (e.status === 422 && e.code === 'validation_failed') {
        const mapped = mapDetails(e.details, meta);
        setErrors(mapped.errors);
        setFailure({ message: `The document was not ${verb}.`, details: mapped.unmapped, reload: false });
      } else if (e.code === 'conflict') {
        setErrors({});
        setFailure({
          message: 'Someone else changed this document after you opened it. Your edits are kept below and nothing was saved.',
          details: [],
          reload: true,
        });
      } else {
        setErrors({});
        setFailure({
          message: e.message,
          details: e.details,
          reload: e.code === 'invalid_docstatus',
        });
      }
    } else {
      setErrors({});
      setFailure({ message: 'Cannot reach the server. Try again.', details: [], reload: false });
    }
    setAttempt((n) => n + 1);
  };

  const run = async (verb: string, work: () => Promise<void>) => {
    setBusy(true);
    setStatus('');
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
      setFailure({ message: 'Fix the fields marked below. The document was not saved.', details: [], reload: false });
      setAttempt((n) => n + 1);
      return;
    }
    if (!isNew && Object.keys(built.data).length === 0) {
      setStatus('There are no changes to save.');
      return;
    }
    void run('saved', async () => {
      if (name === null) {
        const created = await createDocument(meta.name, built.data);
        leavingRef.current = true;
        navigate(formHref(meta.name, String(created.name)));
      } else {
        const updated = await updateDocument(meta.name, name, version, built.data);
        adopt(updated);
        setStatus('Saved.');
      }
    });
  };

  const lifecycle = (action: LifecycleAction) => {
    if (busy || name === null) return;
    if (dirty) {
      setErrors({});
      setStatus('');
      setFailure({ message: 'You have unsaved changes. Save them first.', details: [], reload: false });
      setAttempt((n) => n + 1);
      return;
    }
    if (action === 'delete') {
      setAsking({ kind: 'delete' });
      return;
    }
    void run(TRANSITION_VERB[action], async () => {
      const result = await transitionDocument(meta.name, name, action, version);
      if (action === 'amend') {
        leavingRef.current = true;
        navigate(formHref(meta.name, String(result.name)));
        return;
      }
      adopt(result);
      setStatus(TRANSITION_DONE[action]);
      // The button that was used is gone: put the focus on the heading, which names the document.
      headingRef.current?.focus();
    });
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
      setStatus('Loaded the current version.');
    });
  };

  const setValue = (field: string, value: FormValue) => {
    setState((s) => ({ ...s, values: { ...s.values, [field]: value } }));
    setErrors((e) => (field in e ? Object.fromEntries(Object.entries(e).filter(([k]) => k !== field)) : e));
  };

  const title = isNew ? `New ${meta.label}` : (name ?? meta.label);
  const docStatus = ctx.docStatus;

  // Problems that no single control carries: shown in the alert next to the unmapped details.
  const tableLevel = meta.fields
    .filter((f) => f.type === 'table' && errors[errorKey(f.name)] !== undefined)
    .map((f) => `${f.label}: ${errors[errorKey(f.name)]}`);

  return (
    <main>
      <p>
        <a href={listHref(meta.name)}>Back to {meta.label} list</a>
      </p>
      <h1 ref={headingRef} tabIndex={-1}>
        {title}
      </h1>
      {meta.submittable && !isNew ? (
        <p className="doc-status">
          Status: <strong>{docStatusLabel(docStatus)}</strong>
        </p>
      ) : null}

      {failure ? (
        <div role="alert" tabIndex={-1} ref={alertRef} className="alert">
          <p className="error">{failure.message}</p>
          {failure.details.length + tableLevel.length > 0 ? (
            <ul>
              {[...failure.details, ...tableLevel].map((d) => (
                <li key={d}>{d}</li>
              ))}
            </ul>
          ) : null}
          {failure.reload ? (
            <button
              type="button"
              onClick={() => (dirty ? setAsking({ kind: 'reload' }) : reload())}
            >
              Reload
            </button>
          ) : null}
        </div>
      ) : null}
      {/* Always in the page, so that a screen reader announces a change. */}
      <p role="status" aria-live="polite" className="status-line">
        {busy ? 'Working...' : status}
      </p>

      {asking?.kind === 'delete' ? (
        <Prompt
          message={`Delete ${name ?? 'this document'}? This cannot be undone.`}
          confirmLabel="Delete"
          cancelLabel="Keep"
          onConfirm={confirmDelete}
          onCancel={() => setAsking(null)}
        />
      ) : null}
      {asking?.kind === 'reload' ? (
        <Prompt
          message="Reloading discards your unsaved edits."
          confirmLabel="Discard edits and reload"
          cancelLabel="Keep editing"
          onConfirm={reload}
          onCancel={() => setAsking(null)}
        />
      ) : null}
      {asking?.kind === 'leave' ? (
        <Prompt
          message="You have unsaved changes."
          confirmLabel="Leave and discard"
          cancelLabel="Stay"
          onConfirm={() => leaveTo(asking.next)}
          onCancel={() => setAsking(null)}
        />
      ) : null}

      {isNew && !canSave ? (
        <p role="alert" className="error">
          You cannot create {meta.label} documents.
        </p>
      ) : (
        <form ref={formRef} noValidate onSubmit={save}>
          <div className="actions">
            {canSave ? (
              <button type="submit" className="primary" ref={saveRef} aria-disabled={busy || undefined}>
                Save
              </button>
            ) : null}
            {actions.map((action) => (
              <button
                key={action}
                type="button"
                className={action === 'delete' ? 'danger' : undefined}
                aria-disabled={busy || undefined}
                onClick={() => lifecycle(action)}
              >
                {action === 'submit' ? 'Submit' : action === 'cancel' ? 'Cancel' : action === 'amend' ? 'Amend' : 'Delete'}
              </button>
            ))}
          </div>
          {canSave ? <p className="hint">* required</p> : null}

          <div className="fields">
            {meta.fields.map((field) => {
              const id = `${idBase}-${field.name}`;
              const message = errors[errorKey(field.name)];
              const readOnly = readOnlyOf(field);
              if (field.type === 'table') {
                return (
                  <fieldset key={field.name} className="field table-field">
                    <legend>
                      {field.label}
                      {field.required ? <span aria-hidden="true"> *</span> : null}
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
                  {field.required ? <span aria-hidden="true"> *</span> : null}
                </label>
              );
              return (
                <div key={field.name} className={field.type === 'check' ? 'field check' : 'field'}>
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
                    <p id={`${id}-error`} className="error field-error">
                      {message}
                    </p>
                  ) : null}
                </div>
              );
            })}
          </div>
        </form>
      )}

      {doc ? <Details meta={meta} doc={doc} /> : null}
    </main>
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
    <section aria-labelledby="details-heading" className="details">
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
