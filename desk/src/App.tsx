import { useEffect, useMemo, useState } from 'react';

import { ApiError, getMeta, getSession, logout, setUnauthenticatedHandler } from './api';
import { Layout } from './components/Layout';
import { Logo } from './components/Logo';
import { ToastProvider } from './components/Toast';
import { Form } from './pages/Form';
import { Home } from './pages/Home';
import { List } from './pages/List';
import { Login } from './pages/Login';
import { clearRecent } from './recent';
import { homeHref, loginHref, navigate, parseHash, useHash } from './router';
import type { Route } from './router';
import type { DocTypeSummary, SessionInfo } from './types';

/** The DocTypes the user can read (sidebar and home page), loaded once after login. */
export type DocTypes = { status: 'loading' } | { status: 'ready'; list: DocTypeSummary[] } | { status: 'error'; message: string };

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
    setUnauthenticatedHandler(() => {
      clearRecent();
      setAuth({ status: 'out' });
    });
    let cancelled = false;
    // On start (and after a reload) ask the server who we are; this also gives the page a
    // fresh CSRF token, which is kept in memory only.
    getSession().then(
      (session) => {
        if (!cancelled) setAuth({ status: 'in', username: session.username });
      },
      (e: unknown) => {
        if (cancelled) return;
        if (e instanceof ApiError && e.status === 401) {
          clearRecent();
          setAuth({ status: 'out' });
        }
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

  const [doctypes, setDoctypes] = useState<DocTypes>({ status: 'loading' });
  const loggedIn = auth.status === 'in';
  useEffect(() => {
    if (!loggedIn) return;
    let cancelled = false;
    getMeta().then(
      (list) => {
        if (!cancelled) setDoctypes({ status: 'ready', list });
      },
      (e: unknown) => {
        if (!cancelled) {
          setDoctypes({ status: 'error', message: e instanceof ApiError ? e.message : 'Cannot reach the server.' });
        }
      },
    );
    return () => {
      cancelled = true;
      setDoctypes({ status: 'loading' });
    };
  }, [loggedIn]);

  const onLoggedIn = (session: SessionInfo) => {
    setAuth({ status: 'in', username: session.username });
    navigate(homeHref());
  };

  const onLogout = () => {
    const out = () => {
      clearRecent();
      setAuth({ status: 'out' });
    };
    logout().then(out, out);
  };

  if (auth.status === 'loading') {
    return (
      <main className="splash">
        <Logo size={40} />
        <p>Loading...</p>
      </main>
    );
  }
  if (auth.status === 'error') {
    return (
      <main className="splash">
        <Logo size={40} />
        <p role="alert" className="alert">
          {auth.message}
        </p>
      </main>
    );
  }

  return (
    <ToastProvider>
      {auth.status === 'out' ? (
        <Login onLoggedIn={onLoggedIn} />
      ) : (
        <Layout
          username={auth.username}
          route={route}
          doctypes={doctypes.status === 'ready' ? doctypes.list : []}
          onLogout={onLogout}
        >
          <Page route={route} username={auth.username} doctypes={doctypes} />
        </Layout>
      )}
    </ToastProvider>
  );
}

function Page({ route, username, doctypes }: { route: Route; username: string; doctypes: DocTypes }) {
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
      return <Home doctypes={doctypes} />;
    case 'notfound':
      return (
        <div className="page">
          <h1>Not found</h1>
          <p>
            <a href={homeHref()}>Go to the home page</a>
          </p>
        </div>
      );
  }
}
