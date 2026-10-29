import { listHref } from '../router';

interface Props {
  doctype: string;
  /** The document's name, or null for "New". */
  docName: string | null;
}

/** A placeholder: the form view arrives in M4.2. */
export function Form({ doctype, docName }: Props) {
  return (
    <main>
      <h1>{docName === null ? `New ${doctype}` : `${doctype}: ${docName}`}</h1>
      <p>The form view arrives in M4.2.</p>
      <p>
        <a href={listHref(doctype)}>Back to the list</a>
      </p>
    </main>
  );
}
