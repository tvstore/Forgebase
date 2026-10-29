# 0010 Authentication and sessions

- Status: Accepted
- Date: 2026-10-29 (owner decisions of the M3 plan; implementation details of M3.1 separated below)

## Context

M3 adds users, authorization and a REST API (see [milestones](../blueprint/milestones.md#m3-auth-permissions-rest-api) and the [security model](../blueprint/security-model.md)). Before permissions (M3.2) and HTTP (M3.3) can be built, Forgebase needs a way to store users and credentials, to start and end sessions, to issue API tokens for machine clients, and to resist password guessing. [ADR 0005](0005-http-stack.md) chose axum on tokio; the security model already leans towards opaque server-side sessions and Argon2id, and avoids JWT for first-party browser sessions because of revocation.

M3.1 implements the storage and checking side (`forgebase_db::auth`, framework migration 0003, `forgebase user` and `forgebase token`). Cookies, headers and the HTTP login endpoints come with M3.3; permissions and the Administrator semantics with M3.2.

## Decision

The owner approved the M3 plan on 2026-10-29. The decisions that concern authentication are 3 to 7 and 12:

- **Opaque server-side sessions, not JWT.** A session is a random token whose SHA-256 digest is stored in `fb_session` with the user, a CSRF token digest and two expiry times. A session can be revoked at any moment by deleting its row (decision 3).
- **Session lifetime.** Absolute expiry 7 days after login, idle expiry 24 hours after the last use (decision 4).
- **Argon2id for passwords** with the `argon2` crate, memory 19456 KiB, 2 iterations, 1 lane, a random salt, stored as a PHC string; the parameters inside the string are used to verify, so they can be raised later (decision 5; with `getrandom` for random bytes).
- **Token storage.** Session tokens, CSRF tokens and API secrets are stored as SHA-256 digests only, computed by PostgreSQL (no hash crate). API tokens are a public key plus a secret; the secret is shown once at creation (decision 5).
- **Login lockout.** Failed logins are counted per submitted username, also for unknown users; the fifth failure locks the username for 15 minutes. The failure accounting is committed even though the login fails (decision 6).
- **Uniform failure.** An unknown user, a disabled user, a wrong password and a locked account all produce the same error, and unknown or disabled users still cost one password verification (decision 6).
- **Rotation and revocation.** Every login creates a new session. Changing a password or disabling a user deletes all sessions of that user; logout deletes the session (decision 7).
- **Secrets are never stored, logged or printed**, except the one-time return value of login, of API token creation and of `forgebase token create` (decision 12).

## Implementation details (M3.1)

These were chosen while implementing M3.1; they are implementation details to be reviewed with that change, not owner decisions.

- **Tables** (migration 0003): `fb_user` (`id` UUID v7, `username` unique, `password_hash`, `enabled`), `fb_user_role` (`user_id`, `role`; plain role names), `fb_session` (`token_hash`, `csrf_hash`, `user_id`, `created_at`, `last_seen_at`, `expires_at`), `fb_api_token` (`key`, `secret_hash`, `user_id`, `revoked_at`), `fb_login_failure` (`username`, `failures`, `locked_until`). Deleting a user cascades to its roles, sessions and tokens.
- **Token format.** Session token, CSRF token and API secret: 32 bytes from `getrandom` as 64 lowercase hex characters. API key: 16 bytes as 32 hex characters. Hex was chosen over base64url because it needs no encoding code and is safe in cookies, headers and URLs. A value of another shape is rejected without a database query.
- **Rules.** Username: 1 to 140 characters, ASCII letters, digits and `_ . @ -`, case-sensitive. Role: 1 to 140 characters, not blank, no control characters. Password: 8 to 1024 bytes (the upper bound keeps a client from making the server hash megabytes; there is no complexity rule or breach check yet).
- **Argon2 off the executor.** Hashing and verification run in `tokio::task::spawn_blocking`. `tokio` is now a normal dependency of `forgebase-db` (it was a dev-dependency; already in the tree through tokio-postgres) for that reason only.
- **Dummy hash.** A hash of a random password nobody knows is computed once per process (`OnceLock`) and verified for unknown and disabled users and for locked usernames, so those paths spend comparable time. This reduces timing differences; it does not remove all of them (database round trips differ).
- **Lockout mechanics.** `login` runs in its own transaction. The first statement is `INSERT ... ON CONFLICT DO UPDATE ... RETURNING`, which creates or locks the username's `fb_login_failure` row in one step; the lock is held until the outcome is known, so concurrent attempts for one username are serialized and cannot exceed the limit. While locked, the password is not checked (a dummy verification is run) and the lock is not extended. Failure counts commit with the failed login: call `login` on a pooled client, **not** inside a caller-owned transaction that may roll back (it would undo the counting). Success deletes the row. A username or password that breaks the rules fails the same way without creating a row. The counter keys are the submitted names, so an attacker can create one small row per random username; automatic clean-up is not implemented (rows of unlocked usernames can be deleted by hand or by a later maintenance job). Throttling by client address belongs to the HTTP layer (M3.3).
- **Session validation** is one `UPDATE ... RETURNING` that checks the digest, the user's `enabled` flag and both expiry times and touches `last_seen_at` (one small write per request). An expired session found this way is deleted. `check_csrf` compares the CSRF digest in SQL, requires the session to be valid and does not touch it. Digests are compared by PostgreSQL (an indexed equality lookup, not a constant-time comparison); with 256-bit random tokens this is accepted.
- **API tokens** do not expire; they stop working when revoked or when the user is disabled. Setting a password does not revoke them.
- **Password change** does not clear a lockout; disabling does not either.
- **CLI.** Passwords are read from the first line of standard input (at most 4096 bytes read), never from an argument or environment variable. `forgebase token create` prints `key:secret` and nothing else. The CLI remains a trusted local tool.

## Alternatives considered

- **JWT or other stateless tokens.** Rejected: revoking one needs a deny list or short lifetimes, which brings back server state with more moving parts. Server-side sessions revoke by deleting a row.
- **bcrypt or scrypt.** bcrypt truncates input at 72 bytes and is not memory-hard (general knowledge, Unverified here); scrypt is memory-hard but Argon2id is the current default recommendation (OWASP password storage cheat sheet; recorded as Unverified in the [dependency evaluation](../blueprint/dependency-evaluation.md#8-authentication)). Argon2id's parameters are stored in the hash, which makes later tuning easy.
- **A session store crate** (`tower-sessions`, `axum-login`). More dependencies (the Postgres store pulls sqlx), 0.x releases with breaking changes, and abstractions we do not need; about 200 lines of SQL and Rust cover it.
- **A hash crate for token digests** (`sha2`). Not needed: PostgreSQL computes SHA-256 already, as it does for migration checksums.
- **Lockout kept in memory.** Would not survive restarts or work across several processes; the database row also serializes concurrent attempts.
- **Locking by client address.** The address is only known to the HTTP layer; it is added in M3.3 and does not replace the per-username lock.

## Consequences

- Two new direct dependencies, `argon2` and `getrandom` (see the [dependency evaluation](../blueprint/dependency-evaluation.md)), and `tokio` in `forgebase-db` as a normal dependency.
- The lockout lets anyone who knows a username lock that user out for 15 minutes with five bad attempts (denial of service). This is accepted for M3.1; address-based throttling and an administrator override are follow-up work.
- Argon2 with these parameters uses about 19 MiB of memory per verification, so many parallel logins need bounded concurrency; the HTTP layer must limit it (M3.3).
- A user's password, roles and sessions are not audited or versioned.
- Raising the Argon2 parameters later needs no migration for verification; rehashing old hashes on login is not implemented.
- `forgebase_db::auth` has no permission checks. Anyone with database access or the trusted CLI can create users and tokens.
