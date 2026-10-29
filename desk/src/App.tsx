import { useEffect, useMemo, useState } from 'react';

import { ApiError, getSession, logout, setUnauthenticatedHandler } from './api';
import { Form } from './pages/Form';
import { Home } from './pages/Home';
import { List } from './pages/List';
import { Login } from './pages/Login';
import { homeHref, loginHref, navigate, parseHash, useHash } from './router';
import type { Route } from './router';
import type { SessionInfo } from './types';

type Auth =
  | { status: 'loading' }
  | { status: 'in'; username: string }
  | { status: 'out' }
  | { status: 'error'; message: string };

export function App() {
  const hash = useHash();
  const route = useMemo(() => parseHash(hash), [hash]);
  const [auth, setAuth] = useState<Auth>({ status: 'loading' });

  useEffect(() => {
    // Any 401 on a later request means the session ended: back to the login page.
    setUnauthenticatedHandler(() => setAuth({ status: 'out' }));
    let cancelled = false;
    // On start (and after a reload) ask the server who we are; this also gives the page a
    // fresh CSRF token, which is kept in memory only.
    getSession().then(
      (session) => {
        if (!cancelled) setAuth({ status: 'in', username: session.username });
      },
      (e: unknown) => {
        if (cancelled) return;
        if (e instanceof ApiError && e.status === 401) setAuth({ status: 'out' });
        else setAuth({ status: 'error', message: 'Cannot reach the server. Reload to try again.' });
      },
    );
    return () => {
      cancelled = true;
      setUnauthenticatedHandler(null);
    };
  }, []);

  // Keep the address in line with the state: logged out is the login page, logged in is not.
  const wanted: 'login' | 'app' | null =
    auth.status === 'out' ? 'login' : auth.status === 'in' ? 'app' : null;
  useEffect(() => {
    if (wanted === 'login' && route.name !== 'login') navigate(loginHref(), true);
    if (wanted === 'app' && route.name === 'login') navigate(homeHref(), true);
  }, [wanted, route.name]);

  const onLoggedIn = (session: SessionInfo) => {
    setAuth({ status: 'in', username: session.username });
    navigate(homeHref());
  };

  const onLogout = () => {
    logout().then(
      () => setAuth({ status: 'out' }),
      () => setAuth({ status: 'out' }),
    );
  };

  if (auth.status === 'loading') {
    return (
      <main>
        <p>Loading...</p>
      </main>
    );
  }
  if (auth.status === 'error') {
    return (
      <main>
        <p role="alert" className="error">
          {auth.message}
        </p>
      </main>
    );
  }
  if (auth.status === 'out') return <Login onLoggedIn={onLoggedIn} />;

  return (
    <>
      <header className="app-header">
        <a className="brand" href={homeHref()}>
          Forgebase Desk
        </a>
        <nav aria-label="Main">
          <a href={homeHref()}>Home</a>
        </nav>
        <span className="user">{auth.username}</span>
        <button type="button" onClick={onLogout}>
          Log out
        </button>
      </header>
      <Page route={route} username={auth.username} />
    </>
  );
}

function Page({ route, username }: { route: Route; username: string }) {
  switch (route.name) {
    case 'list':
      return <List key={route.doctype} doctype={route.doctype} params={route.params} />;
    case 'new':
      return <Form key={`new/${route.doctype}`} doctype={route.doctype} docName={null} username={username} />;
    case 'form':
      return (
        <Form
          key={`form/${route.doctype}/${route.docName}`}
          doctype={route.doctype}
          docName={route.docName}
          username={username}
        />
      );
    case 'login': // being redirected to the home page
    case 'home':
      return <Home />;
    case 'notfound':
      return (
        <main>
          <h1>Not found</h1>
          <p>
            <a href={homeHref()}>Go to the home page</a>
          </p>
        </main>
      );
  }
}
