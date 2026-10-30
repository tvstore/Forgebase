import type { DocTypes } from '../App';
import { EmptyState } from '../components/EmptyState';
import { Skeleton } from '../components/Skeleton';
import { groupByApp } from '../nav';
import { listHref, newHref } from '../router';

/** The DocTypes the user can read, as one card per app, each with a "New" shortcut when allowed. */
export function Home({ doctypes }: { doctypes: DocTypes }) {
  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>Home</h1>
          <p className="subtitle">Choose a document type to work with.</p>
        </div>
      </header>
      {doctypes.status === 'loading' ? (
        <div className="card-grid" aria-busy="true">
          <div className="card">
            <Skeleton lines={3} />
          </div>
          <div className="card">
            <Skeleton lines={3} />
          </div>
        </div>
      ) : null}
      {doctypes.status === 'error' ? (
        <p role="alert" className="alert">
          {doctypes.message}
        </p>
      ) : null}
      {doctypes.status === 'ready' ? (
        doctypes.list.length === 0 ? (
          <EmptyState title="Nothing to open yet">
            You cannot read any document type. Ask an administrator to give your account a role with access.
          </EmptyState>
        ) : (
          <div className="card-grid">
            {groupByApp(doctypes.list).map((group) => (
              <section key={group.app} className="card" aria-labelledby={`app-${group.app}`}>
                <h2 id={`app-${group.app}`} className="card-title">
                  {group.app}
                </h2>
                <ul className="doctype-list">
                  {group.doctypes.map((d) => (
                    <li key={d.name}>
                      <a className="doctype-link" href={listHref(d.name)}>
                        {d.label}
                      </a>
                      {d.rights.create !== 'none' ? (
                        <a className="btn btn-small" href={newHref(d.name)} aria-label={`New ${d.label}`}>
                          <span aria-hidden="true">+ </span>New
                        </a>
                      ) : null}
                    </li>
                  ))}
                </ul>
              </section>
            ))}
          </div>
        )
      ) : null}
    </div>
  );
}
