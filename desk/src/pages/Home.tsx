import { useEffect, useState } from 'react';

import { ApiError, getMeta } from '../api';
import { listHref } from '../router';
import type { DocTypeSummary } from '../types';

type State = { doctypes: DocTypeSummary[] } | { error: string } | null;

/** The DocTypes the user can read, from `GET /api/v1/meta`. */
export function Home() {
  const [state, setState] = useState<State>(null);

  useEffect(() => {
    let cancelled = false;
    getMeta().then(
      (doctypes) => {
        if (!cancelled) setState({ doctypes });
      },
      (e: unknown) => {
        if (!cancelled) setState({ error: e instanceof ApiError ? e.message : 'Cannot reach the server.' });
      },
    );
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <main>
      <h1>Document types</h1>
      {state === null ? <p>Loading...</p> : null}
      {state && 'error' in state ? (
        <p role="alert" className="error">
          {state.error}
        </p>
      ) : null}
      {state && 'doctypes' in state ? (
        state.doctypes.length === 0 ? (
          <p>You cannot read any document type.</p>
        ) : (
          <ul className="doctypes">
            {state.doctypes.map((d) => (
              <li key={d.name}>
                <a href={listHref(d.name)}>{d.label}</a>
              </li>
            ))}
          </ul>
        )
      ) : null}
    </main>
  );
}
