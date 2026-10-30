import { useEffect, useId, useRef, useState } from 'react';
import type { ReactNode } from 'react';

import { breadcrumbs } from '../nav';
import { homeHref } from '../router';
import type { Route } from '../router';
import type { DocTypeSummary } from '../types';
import { Breadcrumbs } from './Breadcrumbs';
import { Logo } from './Logo';
import { MenuButton } from './MenuButton';
import { Sidebar } from './Sidebar';

interface Props {
  username: string;
  route: Route;
  /** The DocTypes the user can read (empty until they are loaded). */
  doctypes: DocTypeSummary[];
  onLogout: () => void;
  children: ReactNode;
}

/**
 * The frame of every page after login: a skip link, the top bar (logo, menu button on narrow
 * screens, user menu), the sidebar, the breadcrumbs and the `main` landmark. Below 900 px the
 * sidebar is behind the menu button (`aria-expanded`) and closes when the page changes.
 */
export function Layout({ username, route, doctypes, onLogout, children }: Props) {
  const [open, setOpen] = useState(false);
  const sidebarId = useId();
  const main = useRef<HTMLElement>(null);
  const toggle = useRef<HTMLButtonElement>(null);

  // A new page closes the drawer (state is adjusted while rendering, not in an effect).
  const pageKey = route.name === 'list' || route.name === 'new' || route.name === 'form' ? `${route.name}/${route.doctype}` : route.name;
  const [seenPage, setSeenPage] = useState(pageKey);
  if (seenPage !== pageKey) {
    setSeenPage(pageKey);
    setOpen(false);
  }

  // An open drawer takes the focus (its first link) and gives it back to the Menu button on Escape.
  useEffect(() => {
    if (!open) return;
    document.getElementById(sidebarId)?.querySelector<HTMLElement>('a')?.focus();
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      setOpen(false);
      toggle.current?.focus();
    };
    document.addEventListener('keydown', onKey);
    return () => document.removeEventListener('keydown', onKey);
  }, [open, sidebarId]);

  return (
    <div className="shell">
      <a
        className="skip-link"
        href="#content"
        onClick={(event) => {
          // The address is the router's: move the focus instead of following the link.
          event.preventDefault();
          main.current?.focus();
        }}
      >
        Skip to content
      </a>
      <header className="topbar">
        <button
          type="button"
          ref={toggle}
          className="btn btn-ghost nav-toggle"
          aria-expanded={open}
          aria-controls={sidebarId}
          onClick={() => setOpen((o) => !o)}
        >
          <span className="burger" aria-hidden="true" />
          <span className="nav-label">Menu</span>
        </button>
        <a className="brand" href={homeHref()}>
          <Logo />
          <span>Forgebase</span>
        </a>
        <MenuButton label={<span className="user-name">{username}</span>} ariaLabel={`Account: ${username}`} className="btn btn-ghost user-button">
          <button type="button" className="menu-item" onClick={onLogout}>
            Log out
          </button>
        </MenuButton>
      </header>
      <div className="body">
        {open ? <div className="backdrop" aria-hidden="true" onClick={() => setOpen(false)} /> : null}
        <Sidebar id={sidebarId} open={open} doctypes={doctypes} route={route} onNavigate={() => setOpen(false)} />
        <main id="content" ref={main} tabIndex={-1} className="content">
          <Breadcrumbs crumbs={breadcrumbs(route, doctypes)} />
          {children}
        </main>
      </div>
    </div>
  );
}
