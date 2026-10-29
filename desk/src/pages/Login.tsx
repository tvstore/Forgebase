import { useState } from 'react';
import type { FormEvent } from 'react';

import { ApiError, login } from '../api';
import type { SessionInfo } from '../types';

interface Props {
  onLoggedIn: (session: SessionInfo) => void;
}

export function Login({ onLoggedIn }: Props) {
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = (event: FormEvent) => {
    event.preventDefault();
    setBusy(true);
    setError(null);
    login(username, password).then(
      (session) => {
        setPassword('');
        onLoggedIn(session);
      },
      (e: unknown) => {
        setBusy(false);
        // The server's message is uniform ("invalid username or password"): shown as it is.
        setError(e instanceof ApiError ? e.message : 'Cannot reach the server. Try again.');
      },
    );
  };

  return (
    <main className="login">
      <h1>Forgebase</h1>
      <form onSubmit={submit}>
        <label htmlFor="username">Username</label>
        <input
          id="username"
          name="username"
          autoComplete="username"
          autoFocus
          required
          value={username}
          onChange={(e) => setUsername(e.target.value)}
        />
        <label htmlFor="password">Password</label>
        <input
          id="password"
          name="password"
          type="password"
          autoComplete="current-password"
          required
          value={password}
          onChange={(e) => setPassword(e.target.value)}
        />
        {error ? (
          <p role="alert" className="error">
            {error}
          </p>
        ) : null}
        <button type="submit" className="primary" disabled={busy}>
          Log in
        </button>
      </form>
    </main>
  );
}
