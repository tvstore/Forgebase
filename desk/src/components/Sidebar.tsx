import { groupByApp } from '../nav';
import { homeHref, listHref } from '../router';
import type { Route } from '../router';
import type { DocTypeSummary } from '../types';

interface Props {
  id: string;
  open: boolean;
  doctypes: DocTypeSummary[];
  route: Route;
  /** A link was followed (the drawer closes, also when the address does not change). */
  onNavigate: () => void;
}

/** The DocTypes the user can read, grouped by app. The current one is marked. */
export function Sidebar({ id, open, doctypes, route, onNavigate }: Props) {
  const current = route.name === 'list' || route.name === 'new' || route.name === 'form' ? route.doctype : null;
  return (
    <nav
      id={id}
      className={open ? 'sidebar open' : 'sidebar'}
      aria-label="Document types"
      onClick={(event) => {
        if (event.target instanceof Element && event.target.closest('a')) onNavigate();
      }}
    >
      <ul className="side-list">
        <li>
          <a className="side-link" href={homeHref()} aria-current={route.name === 'home' ? 'page' : undefined}>
            Home
          </a>
        </li>
      </ul>
      {groupByApp(doctypes).map((group) => (
        <section key={group.app} aria-label={group.label}>
          <h2 className="side-heading">{group.label}</h2>
          <ul className="side-list">
            {group.doctypes.map((d) => (
              <li key={d.name}>
                <a
                  className="side-link"
                  href={listHref(d.name)}
                  aria-current={d.name === current ? (route.name === 'list' ? 'page' : 'true') : undefined}
                >
                  {d.label}
                </a>
              </li>
            ))}
          </ul>
        </section>
      ))}
    </nav>
  );
}
